import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { versionTree } from './fixtures/version-tree.mjs';

const repository = path.resolve(import.meta.dirname, '..');

// release.sh needs PlistBuddy and the BSD sed it edits the Gradle file with, so this runs on a Mac.
test('a release bumps every client in step and builds into an empty dist/', { skip: process.platform !== 'darwin' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion release dist '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const repo = versionTree(path.join(root, 'repo'));
  const record = path.join(root, 'dist-when-packaging.json');
  for (const file of ['release.sh', 'check-versions.mjs']) fs.copyFileSync(path.join(repository, 'scripts', file), path.join(repo, 'scripts', file));
  fs.writeFileSync(path.join(repo, 'scripts', 'check.sh'), '#!/usr/bin/env bash\nexit 0\n', { mode: 0o755 });
  // Packaging is the first thing that writes into dist/, so the release is stopped there, with a
  // note of what dist/ held when it started.
  fs.writeFileSync(path.join(repo, 'scripts', 'package-agent.mjs'), `import fs from 'node:fs';
fs.writeFileSync(process.env.LEGION_TEST_RECORD, JSON.stringify(fs.readdirSync(new URL('../dist/', import.meta.url))));
process.exit(1);
`);
  fs.writeFileSync(path.join(repo, '.gitignore'), 'dist/\n');
  fs.writeFileSync(path.join(root, 'gitconfig'), '');
  // A gh that is not signed in, so the dry run asks nothing of GitHub, and the repository has no
  // origin to fetch from: both only become notes of what a real run would stop at.
  fs.mkdirSync(path.join(root, 'bin'));
  fs.writeFileSync(path.join(root, 'bin', 'gh'), '#!/bin/sh\nexit 1\n', { mode: 0o755 });
  const env = { ...process.env, PATH: `${path.join(root, 'bin')}:${process.env.PATH}`, GIT_CONFIG_GLOBAL: path.join(root, 'gitconfig'), GIT_CONFIG_NOSYSTEM: '1', LEGION_TEST_RECORD: record };
  const git = (...args) => {
    const result = spawnSync('git', ['-C', repo, '-c', 'user.name=me', '-c', 'user.email=me@example.invalid', ...args], { encoding: 'utf8', env });
    assert.equal(result.status, 0, result.stderr);
    return result.stdout;
  };
  git('init', '-q', '-b', 'main');
  git('add', '-A');
  git('commit', '-qm', 'Base');
  // What an earlier release leaves behind: the previous agent's bundle.
  fs.mkdirSync(path.join(repo, 'dist'));
  fs.writeFileSync(path.join(repo, 'dist', 'legionctl-agent-3.0.1.tgz'), 'previous agent');
  fs.writeFileSync(path.join(repo, 'dist', 'Legion-Control-agent-manifest.json'), '{"agentVersion":"3.0.1"}\n');
  fs.writeFileSync(path.join(root, 'notes.md'), 'A release.\n');

  const result = spawnSync('bash', [path.join(repo, 'scripts', 'release.sh'), '1.4.0', '--dry-run', '--notes', path.join(root, 'notes.md')], { cwd: root, encoding: 'utf8', env });
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, /A real run would stop here: gh is not signed in/);
  assert.match(result.stderr, /A real run would stop here: cannot fetch main from origin/);
  assert.match(result.stdout, /Versions agree: agent 3\.1\.0 \(floor 3\.1\.0\), app 1\.4\.0 \(build 15\)\./);
  // Nothing an earlier run left, only this run's notes, kept there for a --resume.
  assert.deepEqual(JSON.parse(fs.readFileSync(record, 'utf8')), ['release-notes.md']);
  assert.equal(fs.readFileSync(path.join(repo, 'dist', 'release-notes.md'), 'utf8'), 'A release.\n');
  // The bump is put back when the release does not make it to a commit.
  assert.equal(git('status', '--porcelain'), '');
});
