import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const repository = path.resolve(import.meta.dirname, '..');

// android/build-apk.sh in a scratch checkout, with a JDK, an SDK and a Gradle wrapper that only
// pretend: enough to see which agent bundle the script asks for and what it tells Gradle.
function checkout(t, { agent = '3.1.0', bundle = agent, manifest = bundle } = {}) {
  // bundle: null is a dist/ without any of the bundle, which is every checkout but a release's.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion android build '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const write = (file, text, mode = 0o644) => {
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), text, { mode });
  };
  fs.mkdirSync(path.join(root, 'android'));
  fs.copyFileSync(path.join(repository, 'android', 'build-apk.sh'), path.join(root, 'android', 'build-apk.sh'));
  write('android/app/build.gradle.kts', '// the app module\n');
  write('android/gradlew', `#!/bin/sh
printf '%s\\n' "$@" > "${path.join(root, 'gradle-arguments')}"
mkdir -p app/build/outputs/apk/release
printf 'apk' > app/build/outputs/apk/release/app-release-unsigned.apk
`, 0o755);
  write('agent/package.json', agent === null ? '{ "name": "legionctl" }\n' : JSON.stringify({ name: 'legionctl', version: agent, legionControl: { agentFloor: agent } }, null, 2) + '\n');
  if (bundle !== null) {
    write(`dist/legionctl-agent-${bundle}.tgz`, 'tarball');
    write('dist/Legion-Control-agent-manifest.json', JSON.stringify({ schema: 1, version: '1.4.0', agentVersion: manifest, artifacts: [] }, null, 2) + '\n');
    write('dist/Legion-Control-agent-manifest.json.sig', 'signature\n');
  }
  write('jdk/bin/java', "#!/bin/sh\necho '    java.specification.version = 21' >&2\n", 0o755);
  write('jdk/bin/keytool', '#!/bin/sh\nwhile [ $# -gt 0 ]; do [ "$1" = -keystore ] && : > "$2"; shift; done\n', 0o755);
  write('sdk/build-tools/35.0.0/zipalign', '#!/bin/sh\ncp "$4" "$5"\n', 0o755);
  write('sdk/build-tools/35.0.0/apksigner', `#!/bin/sh
[ "$1" = verify ] && { echo Verified; exit 0; }
while [ $# -gt 1 ]; do [ "$1" = --out ] && out="$2"; shift; done
cp "$1" "$out"
`, 0o755);
  fs.mkdirSync(path.join(root, 'home'));
  return root;
}

function build(root, ...args) {
  const env = { ...process.env, HOME: path.join(root, 'home'), LEGION_ANDROID_JAVA_HOME: path.join(root, 'jdk'), ANDROID_SDK_ROOT: path.join(root, 'sdk') };
  delete env.ANDROID_HOME;
  return spawnSync('bash', [path.join(root, 'android', 'build-apk.sh'), ...args], { encoding: 'utf8', env });
}
const gradleArguments = (root) => fs.readFileSync(path.join(root, 'gradle-arguments'), 'utf8').trim().split('\n');

test('the APK build bundles the agent agent/package.json names and tells Gradle which one', { skip: process.platform === 'win32' }, (t) => {
  for (const args of [[], ['--require-agent']]) {
    const root = checkout(t);
    const result = build(root, ...args);
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /Bundling the signed agent 3\.1\.0 from dist\//);
    assert.deepEqual(gradleArguments(root), ['--no-daemon', 'assembleRelease', '-Plegion.agentVersion=3.1.0']);
    assert.equal(fs.existsSync(path.join(root, 'android/app/build/outputs/apk/release/app-release.apk')), true);
  }
});

test('without the signed agent bundle the APK builds and says so, unless a release requires it', { skip: process.platform === 'win32' }, (t) => {
  // Anyone without the release key: no dist/ at all.
  const contributor = checkout(t, { bundle: null });
  const result = build(contributor);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /No signed agent bundle in dist\/, so this APK carries no agent and its install action says so\./);
  assert.match(result.stdout, /A release build passes --require-agent/);
  assert.deepEqual(gradleArguments(contributor), ['--no-daemon', 'assembleRelease', '-Plegion.agentVersion=3.1.0']);
  assert.equal(fs.existsSync(path.join(contributor, 'android/app/build/outputs/apk/release/app-release.apk')), true);

  const release = checkout(t, { bundle: null });
  const required = build(release, '--require-agent');
  assert.equal(required.status, 1);
  assert.match(required.stderr, /missing signed agent artifact: legionctl-agent-3\.1\.0\.tgz\. Run scripts\/package-agent\.mjs first\./);
  assert.equal(fs.existsSync(path.join(release, 'gradle-arguments')), false);
});

test('the APK build refuses a dist/ that holds another agent than agent/package.json', { skip: process.platform === 'win32' }, (t) => {
  // What dist/ looks like after the agent moved on and the bundle was not built again. Part of a
  // bundle is refused with or without --require-agent: the APK would carry a manifest naming a
  // tarball it does not have.
  const stale = checkout(t, { bundle: '3.0.1' });
  const missing = build(stale);
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /missing signed agent artifact: legionctl-agent-3\.1\.0\.tgz/);
  assert.equal(fs.existsSync(path.join(stale, 'gradle-arguments')), false);

  const mixed = checkout(t, { manifest: '3.0.1' });
  const result = build(mixed);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Legion-Control-agent-manifest\.json is for agent 3\.0\.1, not 3\.1\.0/);
  assert.equal(fs.existsSync(path.join(mixed, 'gradle-arguments')), false);

  const unversioned = build(checkout(t, { agent: null, bundle: '3.1.0' }));
  assert.equal(unversioned.status, 1);
  assert.match(unversioned.stderr, /could not read an x\.y\.z agent version/);
});

test('the APK build says why when it cannot read which agent to carry', { skip: process.platform === 'win32' }, (t) => {
  // Under set -e a failed read inside an assignment used to end the script with no message at all.
  const nameless = checkout(t);
  fs.rmSync(path.join(nameless, 'agent', 'package.json'));
  const missing = build(nameless);
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /cannot read .*agent\/package\.json, which names the agent the APK carries/);
  assert.equal(fs.existsSync(path.join(nameless, 'gradle-arguments')), false);

  // Root reads a file whatever its mode, so the rest needs another user.
  if (process.getuid?.() === 0) return;
  const locked = checkout(t);
  fs.chmodSync(path.join(locked, 'agent', 'package.json'), 0o000);
  const unreadable = build(locked);
  assert.equal(unreadable.status, 1);
  assert.match(unreadable.stderr, /cannot read .*agent\/package\.json/);

  const sealed = checkout(t);
  fs.chmodSync(path.join(sealed, 'dist', 'Legion-Control-agent-manifest.json'), 0o000);
  const manifest = build(sealed);
  assert.equal(manifest.status, 1);
  assert.match(manifest.stderr, /Legion-Control-agent-manifest\.json is for agent unknown, not 3\.1\.0/);
  assert.equal(fs.existsSync(path.join(sealed, 'gradle-arguments')), false);
});
