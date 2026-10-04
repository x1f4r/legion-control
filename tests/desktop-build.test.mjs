import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const repository = path.resolve(import.meta.dirname, '..');

// desktop/build.sh in a scratch checkout, with a dotnet that only notes what it was asked to do:
// enough to see whether the build went ahead without the signed agent bundle.
function checkout(t, { bundle = false } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion desktop build '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const write = (file, text, mode = 0o644) => {
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), text, { mode });
  };
  fs.mkdirSync(path.join(root, 'desktop'));
  fs.copyFileSync(path.join(repository, 'desktop', 'build.sh'), path.join(root, 'desktop', 'build.sh'));
  write('desktop/LegionControl.Desktop/LegionControl.Desktop.csproj', '<Project>\n  <PropertyGroup>\n    <Version>1.4.0</Version>\n  </PropertyGroup>\n</Project>\n');
  write('agent/package.json', JSON.stringify({ name: 'legionctl', version: '3.1.0' }, null, 2) + '\n');
  write('dotnet', `#!/bin/sh
[ "$1" = --version ] && { echo 10.0.100; exit 0; }
printf '%s\\n' "$*" >> "${path.join(root, 'dotnet-calls')}"
`, 0o755);
  if (bundle) {
    write('dist/legionctl-agent-3.1.0.tgz', 'tarball');
    write('dist/Legion-Control-agent-manifest.json', '{"agentVersion":"3.1.0"}\n');
    write('dist/Legion-Control-agent-manifest.json.sig', 'signature\n');
  }
  return root;
}
const build = (root, ...args) => spawnSync('bash', [path.join(root, 'desktop', 'build.sh'), ...args], { encoding: 'utf8', env: { ...process.env, LEGION_DOTNET: path.join(root, 'dotnet') } });
const dotnetCalls = (root) => fs.existsSync(path.join(root, 'dotnet-calls')) ? fs.readFileSync(path.join(root, 'dotnet-calls'), 'utf8').trim().split('\n') : [];

test('the desktop build goes ahead without the agent bundle and says so', { skip: process.platform === 'win32' }, (t) => {
  const root = checkout(t);
  const result = build(root, 'test');
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /No signed agent bundle in .*: the agent install action will be unavailable in this build\./);
  assert.equal(dotnetCalls(root).length, 1);
  assert.match(dotnetCalls(root)[0], /^test /);
});

test('with --require-agent the desktop build stops unless every part of the bundle is there', { skip: process.platform === 'win32' }, (t) => {
  const empty = checkout(t);
  const refused = build(empty, 'test', '--require-agent');
  assert.equal(refused.status, 1);
  assert.match(refused.stderr, /Missing signed agent artifact: legionctl-agent-3\.1\.0\.tgz\. Run scripts\/package-agent\.mjs first\./);
  assert.deepEqual(dotnetCalls(empty), []);

  const partial = checkout(t, { bundle: true });
  fs.rmSync(path.join(partial, 'dist', 'Legion-Control-agent-manifest.json.sig'));
  const unsigned = build(partial, '--require-agent', 'test');
  assert.equal(unsigned.status, 1);
  assert.match(unsigned.stderr, /Missing signed agent artifact: Legion-Control-agent-manifest\.json\.sig\./);

  // The flag goes before or after the target alike.
  for (const args of [['test', '--require-agent'], ['--require-agent', 'test']]) {
    const root = checkout(t, { bundle: true });
    const result = build(root, ...args);
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /Bundling the signed agent from /);
    assert.equal(dotnetCalls(root).length, 1);
  }
});
