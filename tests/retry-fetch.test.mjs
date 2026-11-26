import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const REPO_ROOT = path.resolve(import.meta.dirname, '..');
const SH_SCRIPT = path.join(REPO_ROOT, 'scripts', 'retry-fetch.sh');
const PS_SCRIPT = path.join(REPO_ROOT, 'scripts', 'retry-fetch.ps1');

function hasCommand(name) {
  const probe = spawnSync(name, ['--version'], { stdio: 'ignore' });
  return !probe.error;
}

const HAS_PWSH = hasCommand('pwsh');
const HAS_SHELLCHECK = hasCommand('shellcheck');

function makeShFixture(t, scriptBody, { cmdName = 'fake-cmd.sh' } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'retry-fetch-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const counterFile = path.join(dir, 'count.txt');
  const argsLog = path.join(dir, 'args.log');
  const fakeCmd = path.join(dir, cmdName);
  fs.writeFileSync(
    fakeCmd,
    `#!/bin/sh
set -eu
count=0
if [ -f "${counterFile}" ]; then
  count="$(cat "${counterFile}")"
fi
count=$((count + 1))
printf '%s\\n' "$count" > "${counterFile}"
printf '%s\\n' "$*" >> "${argsLog}"
${scriptBody}
`,
    { mode: 0o755 },
  );
  return { dir, counterFile, argsLog, fakeCmd };
}

function makeNodeFixture(t, jsBody) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'retry-fetch-ps-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const counterFile = path.join(dir, 'count.txt');
  const scriptPath = path.join(dir, 'fake-cmd.mjs');
  fs.writeFileSync(
    scriptPath,
    `import fs from 'node:fs';
const counterFile = ${JSON.stringify(counterFile)};
let count = 0;
if (fs.existsSync(counterFile)) {
  count = Number.parseInt(fs.readFileSync(counterFile, 'utf8').trim(), 10) || 0;
}
count += 1;
fs.writeFileSync(counterFile, String(count) + '\\n');
${jsBody}
`,
  );
  return { dir, counterFile, scriptPath };
}

function readCount(counterFile) {
  return Number.parseInt(fs.readFileSync(counterFile, 'utf8').trim(), 10);
}

test('retry-fetch.sh has valid POSIX sh syntax', { skip: process.platform === 'win32' }, () => {
  execFileSync('sh', ['-n', SH_SCRIPT], { encoding: 'utf8' });
});

test('retry-fetch.sh passes ShellCheck at warning severity', { skip: !HAS_SHELLCHECK }, () => {
  execFileSync('shellcheck', ['-S', 'warning', SH_SCRIPT], { encoding: 'utf8' });
});

test(
  'retry-fetch.sh succeeds on first try without retrying when healthy',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeShFixture(t, 'echo "downloaded ok"');
    const res = spawnSync(
      'sh',
      [SH_SCRIPT, '--attempts', '3', '--delays', '0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 0, res.stderr);
    assert.equal(readCount(counterFile), 1);
    assert.equal(res.stdout.trim(), 'downloaded ok');
    assert.doesNotMatch(res.stderr, /retrying/);
  },
);

test(
  'retry-fetch.sh retries a command that fails twice with transient fetch errors then succeeds',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeShFixture(
      t,
      `if [ "$count" -eq 1 ]; then
  echo "E: Failed to fetch http://security.ubuntu.com/ubuntu/pool/main/p/pkg.deb  File has unexpected size (1234 != 5678). Mirror sync in progress?" >&2
  exit 100
fi
if [ "$count" -eq 2 ]; then
  echo "E: Unable to fetch some archives, maybe run apt-get update or try with --fix-missing?" >&2
  exit 100
fi
echo "OK on attempt $count"
`,
    );

    const res = spawnSync(
      'sh',
      [SH_SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 0, res.stderr);
    assert.equal(readCount(counterFile), 3);
    assert.match(res.stdout, /OK on attempt 3/);
    assert.match(res.stderr, /retrying in 0s/);
  },
);

test(
  'retry-fetch.sh bounds attempts and preserves exact exit code when transient errors persist',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeShFixture(
      t,
      `echo "error: failed retrieving file 'core.db' from mirror.example : Operation too slow" >&2
exit 42
`,
    );

    const res = spawnSync(
      'sh',
      [SH_SCRIPT, '--attempts', '3', '--delays', '0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 42);
    assert.equal(readCount(counterFile), 3);
    assert.match(res.stderr, /giving up/);
  },
);

test(
  'retry-fetch.sh does not retry non-fetch packaging, compile, or 404 failures',
  { skip: process.platform === 'win32' },
  (t) => {
    const cases = [
      {
        name: 'apt unmet dependencies on local .deb',
        body: `echo "The following packages have unmet dependencies:" >&2
echo " legion-app : Depends: missing-lib (>= 9.0) but it is not installable" >&2
echo "E: Unable to correct problems, you have held broken packages." >&2
exit 100`,
        expectedExit: 100,
      },
      {
        name: 'dpkg maintainer script failure',
        body: `echo "dpkg: error processing package legion (--configure):" >&2
echo "E: Sub-process /usr/bin/dpkg returned an error code (1)" >&2
exit 100`,
        expectedExit: 100,
      },
      {
        name: 'pacman unsatisfied dependency on local package',
        body: `echo "error: failed to prepare transaction (could not satisfy dependencies)" >&2
exit 1`,
        expectedExit: 1,
      },
      {
        name: 'cargo install Rust compilation error after downloading crates',
        body: `echo "  Downloading crates ..." >&2
echo "error[E0425]: cannot find value foo in this scope" >&2
echo "error: could not compile foo (bin foo)" >&2
exit 101`,
        expectedExit: 101,
      },
      {
        name: 'npm ci lockfile mismatch',
        body: `echo "npm error code EUSAGE" >&2
echo "npm error npm ci can only install packages when your package.json and package-lock.json are in sync." >&2
exit 1`,
        expectedExit: 1,
      },
      {
        name: 'curl permanent HTTP 404',
        body: `echo "curl: (22) The requested URL returned error: 404" >&2
exit 22`,
        expectedExit: 22,
      },
    ];

    for (const c of cases) {
      const { counterFile, fakeCmd } = makeShFixture(t, c.body);
      const res = spawnSync(
        'sh',
        [SH_SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd],
        { encoding: 'utf8' },
      );
      assert.equal(res.status, c.expectedExit, `${c.name}: unexpected exit code`);
      assert.equal(readCount(counterFile), 1, `${c.name}: should not retry`);
      assert.match(res.stderr, /not retrying/, `${c.name}: expected not-retrying notice`);
    }
  },
);

test(
  'retry-fetch.sh treats apt-get update exit 0 with failed index downloads as a retryable failure',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, argsLog, fakeCmd } = makeShFixture(
      t,
      `if [ "$count" -lt 3 ]; then
  echo "W: Some index files failed to download. They have been ignored, or old ones used instead." >&2
  exit 0
fi
echo "Reading package lists..."
exit 0
`,
      { cmdName: 'apt-get' },
    );

    const res = spawnSync(
      'sh',
      [SH_SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd, 'update', '-qq'],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 0, res.stderr);
    assert.equal(readCount(counterFile), 3);
    assert.match(res.stderr, /failed index downloads/);
    const loggedArgs = fs.readFileSync(argsLog, 'utf8').trim().split('\n');
    assert.equal(loggedArgs.length, 3);
    for (const line of loggedArgs) {
      assert.match(line, /-o Acquire::Retries=3 update -qq/);
    }
  },
);

test(
  'retry-fetch.sh fails non-zero when apt-get update always exits 0 with failed index downloads',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeShFixture(
      t,
      `echo "W: Some index files failed to download. They have been ignored, or old ones used instead." >&2
exit 0
`,
      { cmdName: 'apt-get' },
    );

    const res = spawnSync(
      'sh',
      [SH_SCRIPT, '--attempts', '3', '--delays', '0,0', '--', fakeCmd, 'update', '-q'],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 100);
    assert.equal(readCount(counterFile), 3);
    assert.match(res.stderr, /giving up/);
  },
);

test(
  'retry-fetch.sh injects Acquire::Retries=3 and refreshes apt indexes before retrying apt-get install',
  { skip: process.platform === 'win32' },
  (t) => {
    const { argsLog, fakeCmd } = makeShFixture(
      t,
      `case "$*" in
  *" update "*)
    exit 0
    ;;
esac
if [ "$count" -eq 1 ]; then
  echo "E: Unable to fetch some archives, maybe run apt-get update or try with --fix-missing?" >&2
  exit 100
fi
echo "Setting up pkg..."
exit 0
`,
      { cmdName: 'apt-get' },
    );

    const res = spawnSync(
      'sh',
      [SH_SCRIPT, '--attempts', '3', '--delays', '0,0', '--', fakeCmd, 'install', '-y', 'foo'],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 0, res.stderr);
    const loggedArgs = fs.readFileSync(argsLog, 'utf8').trim().split('\n');
    assert.deepEqual(loggedArgs, [
      '-o Acquire::Retries=3 install -y foo',
      '-o Acquire::Retries=3 update -qq',
      '-o Acquire::Retries=3 install -y foo',
    ]);
  },
);

test(
  'retry-fetch.sh --quiet hides output on success and dumps it to stderr on failure',
  { skip: process.platform === 'win32' },
  (t) => {
    const ok = makeShFixture(t, 'echo "noisy success output"');
    const okRes = spawnSync(
      'sh',
      [SH_SCRIPT, '--quiet', '--attempts', '2', '--delays', '0', '--', ok.fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(okRes.status, 0);
    assert.equal(okRes.stdout, '');
    assert.equal(okRes.stderr, '');

    const bad = makeShFixture(t, 'echo "E: Unable to locate package nope" >&2; exit 100');
    const badRes = spawnSync(
      'sh',
      [SH_SCRIPT, '--quiet', '--attempts', '2', '--delays', '0', '--', bad.fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(badRes.status, 100);
    assert.match(badRes.stderr, /Unable to locate package nope/);
    assert.match(badRes.stderr, /not retrying/);
  },
);

test('retry-fetch.ps1 follows repository PowerShell and PSScriptAnalyzer conventions', () => {
  const text = fs.readFileSync(PS_SCRIPT, 'utf8');
  assert.match(text, /^#Requires -Version 5\.1/m);
  assert.match(text, /\[CmdletBinding\(/);
  assert.match(text, /\$VerifyCommand/);
  assert.match(text, /\$ScriptBlock/);
  assert.doesNotMatch(text, /\bWrite-Host\b/);
  assert.doesNotMatch(text, /\$LASTEXITCODE\s*=/);
  assert.match(text, /gateway timeout/i);
});

test(
  'retry-fetch.ps1 retries transient 504 twice then succeeds (with pwsh)',
  { skip: !HAS_PWSH },
  (t) => {
    const { counterFile, scriptPath } = makeNodeFixture(
      t,
      `if (count < 3) {
  console.error('The remote server returned an error: (504) Gateway Timeout.');
  process.exit(1);
}
console.log('installed ok on attempt ' + count);
`,
    );

    const res = spawnSync(
      'pwsh',
      ['-NoProfile', '-File', PS_SCRIPT, '-Attempts', '4', '-Delays', '0,0,0', '--', process.execPath, scriptPath],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 0, res.stderr);
    assert.equal(readCount(counterFile), 3);
    assert.match(res.stdout, /installed ok on attempt 3/);
    assert.match(res.stderr, /retrying in 0s/);
  },
);

test(
  'retry-fetch.ps1 bounds attempts, preserves exit code, and rejects non-fetch failures (with pwsh)',
  { skip: !HAS_PWSH },
  (t) => {
    const alwaysFail = makeNodeFixture(
      t,
      `console.error('504 (Gateway Timeout)');
process.exit(42);
`,
    );
    const failRes = spawnSync(
      'pwsh',
      ['-NoProfile', '-File', PS_SCRIPT, '-Attempts', '3', '-Delays', '0', '--', process.execPath, alwaysFail.scriptPath],
      { encoding: 'utf8' },
    );
    assert.equal(failRes.status, 42);
    assert.equal(readCount(alwaysFail.counterFile), 3);
    assert.match(failRes.stderr, /giving up/);

    const nonFetch = makeNodeFixture(
      t,
      `console.error('error: could not compile broken');
process.exit(101);
`,
    );
    const nonFetchRes = spawnSync(
      'pwsh',
      ['-NoProfile', '-File', PS_SCRIPT, '-Attempts', '3', '-Delays', '0,0', '--', process.execPath, nonFetch.scriptPath],
      { encoding: 'utf8' },
    );
    assert.equal(nonFetchRes.status, 101);
    assert.equal(readCount(nonFetch.counterFile), 1);
    assert.match(nonFetchRes.stderr, /not retrying/);
  },
);

test(
  'retry-fetch.ps1 -VerifyCommand fails when expected tool is missing on PATH even after exit 0 (with pwsh)',
  { skip: !HAS_PWSH },
  (t) => {
    const { counterFile, scriptPath } = makeNodeFixture(t, `console.log('done');`);
    const res = spawnSync(
      'pwsh',
      [
        '-NoProfile',
        '-File',
        PS_SCRIPT,
        '-Attempts',
        '3',
        '-Delays',
        '0,0',
        '-VerifyCommand',
        'legion-definitely-missing-tool-xyz',
        '--',
        process.execPath,
        scriptPath,
      ],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 1);
    assert.equal(readCount(counterFile), 1);
    assert.match(res.stderr, /not found on PATH/);
  },
);
