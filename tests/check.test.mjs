import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const repository = path.resolve(import.meta.dirname, '..');

// scripts/check.sh in a scratch checkout that holds only what a test puts there, run with an npm,
// a node and a Gradle wrapper that note what they were asked to do and do nothing. node runs the
// contract tests for real, which is what the agent leg is tested on here.
function scratch(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-check-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const write = (file, text, mode) => {
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), text, { mode });
  };
  write('scripts/check.sh', fs.readFileSync(path.join(repository, 'scripts', 'check.sh')), 0o755);
  const note = (name) => `printf '%s\\n' "${name} $*" >> "$LEGION_TEST_CALLS"\n`;
  write('bin/npm', `#!/bin/sh\n${note('npm')}`, 0o755);
  write('bin/node', `#!/bin/sh\n${note('node')}case "$1 $2" in\n  "--test contract/test/"*) exec "$LEGION_TEST_NODE" "$@" ;;\nesac\n`, 0o755);
  write('android/gradlew', `#!/bin/sh\n${note('gradlew')}printf '%s\\n' "JAVA_HOME=$JAVA_HOME" >> "$LEGION_TEST_CALLS"\n`, 0o755);
  const calls = path.join(root, 'calls');
  // Without NODE_TEST_CONTEXT, which this test's own runner sets: a node --test that inherits it
  // reports to that runner instead of through its exit status, and then passes whatever failed.
  const inherited = Object.fromEntries(Object.entries(process.env).filter(([name]) => name !== 'NODE_TEST_CONTEXT'));
  const run = (component, env = {}) => {
    fs.rmSync(calls, { force: true });
    const result = spawnSync('bash', [path.join(root, 'scripts', 'check.sh'), component], {
      encoding: 'utf8',
      env: { ...inherited, PATH: `${path.join(root, 'bin')}${path.delimiter}${process.env.PATH}`, LEGION_TEST_CALLS: calls, LEGION_TEST_NODE: process.execPath, ...env },
    });
    return { ...result, calls: fs.existsSync(calls) ? fs.readFileSync(calls, 'utf8').trim().split('\n') : [] };
  };
  return { root, write, run };
}

const PASSING = "import test from 'node:test';\ntest('holds', () => {});\n";
const FAILING = "import assert from 'node:assert/strict';\nimport test from 'node:test';\ntest('a planted failure', () => assert.fail('planted in the contract tests'));\n";

test('the agent check runs the contract tests after the validator and the generator check, and fails with them', { skip: process.platform === 'win32' }, (t) => {
  const { write, run } = scratch(t);
  write('contract/test/passing.test.mjs', PASSING);

  const green = run('agent');
  assert.equal(green.status, 0, green.stderr);
  const at = (call) => green.calls.indexOf(call);
  assert.ok(at('node contract/validate.mjs') >= 0, green.calls.join('\n'));
  assert.ok(at('node contract/tools/generate.mjs --check') > at('node contract/validate.mjs'), green.calls.join('\n'));
  assert.ok(at('node --test contract/test/passing.test.mjs') > at('node contract/tools/generate.mjs --check'), green.calls.join('\n'));
  // The checks of the whole checkout come before any test runs.
  assert.ok(at('node scripts/check-syntax.mjs') >= 0 && at('node scripts/check-syntax.mjs') < at('npm --prefix agent test'), green.calls.join('\n'));

  // CI and the release preflight both run this leg, so one failing contract test stops both.
  write('contract/test/planted.test.mjs', FAILING);
  const red = run('agent');
  assert.notEqual(red.status, 0, red.stdout);
  assert.match(red.stdout, /planted in the contract tests/);
  const all = run('all');
  assert.notEqual(all.status, 0, all.stdout);
  assert.equal(all.calls.some((call) => call.startsWith('gradlew')), false, 'nothing after the failing leg runs');
});

// A JDK that answers the property query the way a real one does, on stderr, for one version.
function jdk(write, name, version) {
  write(`${name}/bin/java`, `#!/bin/sh\nprintf 'Property settings:\\n    java.specification.version = ${version}\\n    java.version = ${version}.0.1\\n\\nopenjdk version "${version}.0.1"\\n' >&2\n`, 0o755);
}

test('the Android check stops before Gradle unless the JDK it runs on is 21', { skip: process.platform === 'win32' }, (t) => {
  const { root, write, run } = scratch(t);
  jdk(write, 'jdk-17', '17');
  jdk(write, 'jdk-21', '21');

  const old = run('android', { LEGION_ANDROID_JAVA_HOME: path.join(root, 'jdk-17') });
  assert.equal(old.status, 1);
  assert.equal(old.stderr, `The Android checks need JDK 21, and ${path.join(root, 'jdk-17')} is Java 17. Point LEGION_ANDROID_JAVA_HOME at a JDK 21.\n`);
  assert.deepEqual(old.calls, []);

  const missing = run('android', { LEGION_ANDROID_JAVA_HOME: path.join(root, 'no-jdk') });
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /no-jdk is no JDK that runs\./);
  assert.deepEqual(missing.calls, []);

  const right = run('android', { LEGION_ANDROID_JAVA_HOME: path.join(root, 'jdk-21') });
  assert.equal(right.status, 0, right.stderr);
  assert.deepEqual(right.calls, ['gradlew :app:testDebugUnitTest :app:testReleaseUnitTest :app:lintDebug --console=plain', `JAVA_HOME=${path.join(root, 'jdk-21')}`]);
});

// Off a Mac nothing picks a JDK: the JAVA_HOME the job set up is the one checked. uname says Linux
// here wherever the test runs.
test('the Android check off a Mac checks the JAVA_HOME it was given', { skip: process.platform === 'win32' }, (t) => {
  const { root, write, run } = scratch(t);
  write('bin/uname', '#!/bin/sh\necho Linux\n', 0o755);
  jdk(write, 'jdk-26', '26');
  const result = run('android', { LEGION_ANDROID_JAVA_HOME: '', JAVA_HOME: path.join(root, 'jdk-26') });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /jdk-26 is Java 26\./);
  assert.deepEqual(result.calls, []);
});
