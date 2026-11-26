import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const REPO_ROOT = path.resolve(import.meta.dirname, '..');
const SCRIPT = path.join(REPO_ROOT, 'scripts', 'retry-gradle.sh');

function makeFixture(t, scriptBody) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'retry-gradle-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const counterFile = path.join(dir, 'count.txt');
  const logFile = path.join(dir, 'build.log');
  const fakeCmd = path.join(dir, 'fake-gradle.sh');
  fs.writeFileSync(
    fakeCmd,
    `#!/usr/bin/env bash
set -euo pipefail
count=0
if [ -f "${counterFile}" ]; then
  count="$(cat "${counterFile}")"
fi
count=$((count + 1))
echo "$count" > "${counterFile}"
${scriptBody}
`,
    { mode: 0o755 },
  );
  return { dir, counterFile, logFile, fakeCmd };
}

function readCount(counterFile) {
  return Number.parseInt(fs.readFileSync(counterFile, 'utf8').trim(), 10);
}

test('retry-gradle.sh has valid bash syntax', () => {
  execFileSync('bash', ['-n', SCRIPT], { encoding: 'utf8' });
});

test(
  'retries on Maven Central HTTP 429 and overwrites --log with the passing attempt',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, logFile, fakeCmd } = makeFixture(
      t,
      `if [ "$count" -lt 3 ]; then
  echo "FAILURE: Build failed with an exception." >&2
  echo "> Could not GET 'https://repo.maven.apache.org/maven2/org/jetbrains/kotlin/kotlin-gradle-plugins-bom/2.3.21/kotlin-gradle-plugins-bom-2.3.21.pom'. Received status code 429 from server: Too Many Requests" >&2
  exit 1
fi
echo "BUILD SUCCESSFUL on attempt $count"
`,
    );

    const res = spawnSync(
      'bash',
      [SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--log', logFile, '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 0, res.stderr);
    assert.equal(readCount(counterFile), 3);
    assert.match(res.stderr, /retrying in 0s/);
    assert.equal(fs.readFileSync(logFile, 'utf8').trim(), 'BUILD SUCCESSFUL on attempt 3');
  },
);

test(
  'retries on transient Gradle wrapper and network fetch failures',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeFixture(
      t,
      `if [ "$count" -eq 1 ]; then
  echo "Downloading https://services.gradle.org/distributions/gradle-9.6.1-bin.zip"
  echo "Exception in thread \\"main\\" java.net.SocketTimeoutException: Read timed out" >&2
  exit 1
fi
if [ "$count" -eq 2 ]; then
  echo "FAILURE: Build failed with an exception." >&2
  echo "> Could not GET 'https://dl.google.com/dl/android/maven2/com/android/tools/build/gradle/9.3.1/gradle-9.3.1.pom'." >&2
  echo "  > Connection reset" >&2
  exit 1
fi
echo "OK"
`,
    );

    const res = spawnSync(
      'bash',
      [SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 0, res.stderr);
    assert.equal(readCount(counterFile), 3);
  },
);

test(
  'stops after --attempts and preserves the failing exit code on persistent 503',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeFixture(
      t,
      `echo "> Could not GET 'https://repo.maven.apache.org/maven2/foo.pom'. Received status code 503 from server: Service Unavailable" >&2
exit 42
`,
    );

    const res = spawnSync(
      'bash',
      [SCRIPT, '--attempts', '3', '--delays', '0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 42);
    assert.equal(readCount(counterFile), 3);
    assert.match(res.stderr, /giving up/);
  },
);

test(
  'does not retry on Kotlin compilation errors',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeFixture(
      t,
      `echo "e: file:///repo/MainActivity.kt:42:9 Unresolved reference: missingSymbol" >&2
echo "FAILURE: Build failed with an exception." >&2
echo "> Execution failed for task ':app:compileUniversalDebugKotlin'." >&2
exit 1
`,
    );

    const res = spawnSync(
      'bash',
      [SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 1);
    assert.equal(readCount(counterFile), 1);
    assert.match(res.stderr, /not retrying/);
  },
);

test(
  'does not retry on unit test failures even if the test stack trace mentions SocketTimeoutException',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeFixture(
      t,
      `echo "TransferSecurityTest > rejectsTimeout FAILED"
echo "    java.net.SocketTimeoutException: Read timed out"
echo "FAILURE: Build failed with an exception." >&2
echo "> There were failing tests. See the report at: file:///repo/build/reports/tests/index.html" >&2
exit 1
`,
    );

    const res = spawnSync(
      'bash',
      [SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 1);
    assert.equal(readCount(counterFile), 1);
    assert.match(res.stderr, /not retrying/);
  },
);

test(
  'does not retry on permanent HTTP 404 Not Found from a repository',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeFixture(
      t,
      `echo "FAILURE: Build failed with an exception." >&2
echo "> Could not GET 'https://repo.maven.apache.org/maven2/com/example/missing/1.0/missing-1.0.pom'. Received status code 404 from server: Not Found" >&2
exit 1
`,
    );

    const res = spawnSync(
      'bash',
      [SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 1);
    assert.equal(readCount(counterFile), 1);
    assert.match(res.stderr, /not retrying/);
  },
);

test(
  'ignores earlier noisy output when the Gradle FAILURE block is a real test failure',
  { skip: process.platform === 'win32' },
  (t) => {
    const { counterFile, fakeCmd } = makeFixture(
      t,
      `echo "Downloading https://services.gradle.org/distributions/gradle-9.6.1-bin.zip"
echo "Test log: simulated HTTP 429 Too Many Requests handling"
echo "FAILURE: Build failed with an exception." >&2
echo "* What went wrong:" >&2
echo "Execution failed for task ':app:testUniversalDebugUnitTest'." >&2
echo "> There were failing tests." >&2
exit 1
`,
    );

    const res = spawnSync(
      'bash',
      [SCRIPT, '--attempts', '4', '--delays', '0,0,0', '--', fakeCmd],
      { encoding: 'utf8' },
    );
    assert.equal(res.status, 1);
    assert.equal(readCount(counterFile), 1);
    assert.match(res.stderr, /not retrying/);
  },
);
