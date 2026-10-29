// The state store, under the conditions that broke the old one.
//
// Eight concurrent writers doing fifty writes each used to end with five keys.
// That is the test at the bottom of this file, and it now has to end with four
// hundred.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';
import { spawnSync } from 'node:child_process';
import { acquireMutex, mutexPath } from '../src/mutex.mjs';
import { fresh, SRC, withHome } from './helpers.mjs';

test('a patch that lands is reported as landed, and reads back', async () => {
  await withHome(async () => {
    const state = await fresh('state.mjs');
    const service = { id: 'demo' };
    const written = state.saveServiceState(service, { pendingVersion: '2.0.0' });
    assert.equal(written.ok, true);
    assert.equal(written.error, null);
    assert.equal(state.serviceState(service).pendingVersion, '2.0.0');
  });
});

test('a failed write is propagated rather than swallowed', async () => {
  await withHome(async (home) => {
    const state = await fresh('state.mjs');
    // A directory where the file has to go: the write cannot possibly succeed.
    fs.mkdirSync(path.join(home, 'state'), { recursive: true });
    fs.mkdirSync(path.join(home, 'state', 'service-demo.json'), { recursive: true });
    const written = state.saveServiceState({ id: 'demo' }, { pendingVersion: '2.0.0' });
    assert.equal(written.ok, false);
    assert.match(written.error, /could not be/);
  });
});

test('two services writing at once do not drop each other', async () => {
  await withHome(async () => {
    const state = await fresh('state.mjs');
    for (let round = 0; round < 20; round += 1) {
      state.saveServiceState({ id: 'a' }, { round });
      state.saveServiceState({ id: 'b' }, { round });
    }
    assert.equal(state.serviceState({ id: 'a' }).round, 19);
    assert.equal(state.serviceState({ id: 'b' }).round, 19);
  });
});

test('a lock held by another process is waited for, then refused', async () => {
  await withHome(async (home) => {
    const state = await fresh('state.mjs');
    const db = mutexPath(path.join(home, 'busy.lock'));
    const mine = acquireMutex(db, { waitMs: 0 });
    assert.equal(mine.ok, true);
    try {
      // Exclusion is the operating system's, so "who holds it" is not a question
      // this code answers, and a stale pid in a file cannot make it answer wrong.
      assert.throws(() => state.acquireFileLock(path.join(home, 'busy.lock'), { waitMs: 100 }), /did not free within/);
    } finally {
      mine.release();
    }
    const now = state.acquireFileLock(path.join(home, 'busy.lock'), { waitMs: 100 });
    now.release();
  });
});

test('a lock note naming a long-dead process blocks nothing', async () => {
  await withHome(async (home) => {
    const state = await fresh('state.mjs');
    // The old protocol read this file, decided the owner was gone, and unlinked
    // it — which is the race that lost writes. It is now display-only.
    fs.mkdirSync(path.join(home, 'state'), { recursive: true });
    fs.writeFileSync(path.join(home, 'state', '.lock'), 'pid=99999999\nnonce=aaaaaaaaaaaaaaaa\n');
    const written = state.saveServiceState({ id: 'demo' }, { pendingVersion: '1.2.3' });
    assert.equal(written.ok, true);
  });
});

test('eight concurrent writers keep every one of their four hundred writes', async () => {
  await withHome(async (home) => {
    const writers = 8;
    const writesEach = 50;
    const worker = `
      import { saveServiceState } from ${JSON.stringify(pathToFileURL(path.join(SRC, 'state.mjs')).href)};
      const id = process.argv[2];
      for (let index = 0; index < ${writesEach}; index += 1) {
        const written = saveServiceState({ id: 'shared' }, { [\`\${id}-\${index}\`]: index });
        if (!written.ok) { console.error(written.error); process.exit(1); }
      }
    `;
    const script = path.join(home, 'writer.mjs');
    fs.writeFileSync(script, worker);

    const children = [];
    for (let index = 0; index < writers; index += 1) {
      children.push(
        new Promise((resolve) => {
          const result = spawnSync(process.execPath, [script, `w${index}`], {
            env: { ...process.env, LEGIONCTL_HOME: home },
            encoding: 'utf8',
            timeout: 120000,
          });
          resolve(result);
        }),
      );
    }
    const results = await Promise.all(children);
    for (const result of results) assert.equal(result.status, 0, result.stderr);

    const state = await fresh('state.mjs');
    const stored = state.serviceState({ id: 'shared' });
    assert.equal(Object.keys(stored).length, writers * writesEach);
    for (let index = 0; index < writers; index += 1) {
      assert.equal(stored[`w${index}-${writesEach - 1}`], writesEach - 1);
    }
  });
});

test('a clock set forward does not make this process a stranger to the records it wrote', async () => {
  const state = await import('../src/state.mjs');
  const written = state.ownStartTime();
  const now = Date.now;
  try {
    // As a machine with no clock of its own does once the network sets it.
    Date.now = () => now() + 60 * 60 * 1000;
    assert.equal(state.ownStartTime(), written);
    assert.equal(state.processIsAlive(process.pid, { startedAt: written }), true);
  } finally {
    Date.now = now;
  }
});

test('the start ticks are read past a command name with spaces and parentheses of its own', async () => {
  const { parseStartTicks } = await import('../src/state.mjs');
  const after = 'S 1 4242 4242 0 -1 4194560 1234 0 0 0 10 5 0 0 20 0 11 0 987654 123456789 4321';
  assert.equal(parseStartTicks(`4242 (node) ${after}`), '987654');
  assert.equal(parseStartTicks(`4242 (a (b) c) ${after}`), '987654');
  for (const text of ['4242 node S 1', '4242 (node) S 1 2', '', null]) assert.equal(parseStartTicks(text), null, String(text));
});

test('on Linux a process is told from a later one with its id by its start ticks, whatever the clock says', { skip: process.platform !== 'linux' }, async () => {
  const state = await import('../src/state.mjs');
  const { spawn } = await import('node:child_process');
  const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' });
  await new Promise((resolve) => child.once('spawn', resolve));
  try {
    const ticks = state.processStartTicks(child.pid);
    assert.match(ticks, /^[0-9]+$/);
    const anHourAgo = new Date(Date.now() - 60 * 60 * 1000).toISOString();
    assert.equal(state.processIsAlive(child.pid, { startedAt: anHourAgo, startTicks: ticks }), true, 'the same ticks are the same process, a clock that moved since or not');
    assert.equal(state.processIsAlive(child.pid, { startedAt: new Date().toISOString(), startTicks: String(Number(ticks) - 1) }), false);
  } finally {
    child.kill('SIGKILL');
  }
});

test('a rename Windows refuses for a moment is tried again, and nowhere else', async () => {
  const state = await fresh('state.mjs');
  const sharing = (times, code = 'EPERM') => {
    let calls = 0;
    return {
      calls: () => calls,
      operation() {
        calls += 1;
        if (calls <= times) throw Object.assign(new Error(`${code}: operation not permitted, rename`), { code });
        return 'renamed';
      },
    };
  };
  for (const code of ['EPERM', 'EACCES', 'EBUSY']) {
    const brief = sharing(2, code);
    assert.equal(state.retryWindowsSharing(brief.operation, { platform: 'win32' }), 'renamed', code);
    assert.equal(brief.calls(), 3);
  }
  const elsewhere = sharing(1);
  assert.throws(() => state.retryWindowsSharing(elsewhere.operation, { platform: 'linux' }), /EPERM/);
  assert.equal(elsewhere.calls(), 1, 'an error that means what it says is not waited out');
  const held = sharing(Infinity);
  assert.throws(() => state.retryWindowsSharing(held.operation, { platform: 'win32', waitMs: 50 }), /EPERM/, 'for a while, not for ever');
  const missing = sharing(1, 'ENOENT');
  assert.throws(() => state.retryWindowsSharing(missing.operation, { platform: 'win32' }), /ENOENT/);
});

test('an atomic write goes through the rename retry', async () => {
  await withHome(async (home) => {
    const state = await fresh('state.mjs');
    const file = path.join(home, 'state', 'retried.json');
    const rename = fs.renameSync;
    let refused = 0;
    try {
      fs.renameSync = (source, target) => {
        if (target === file && refused < 2) {
          refused += 1;
          throw Object.assign(new Error('EBUSY: resource busy or locked, rename'), { code: 'EBUSY' });
        }
        return rename(source, target);
      };
      const written = state.writeJsonAtomic(file, { value: 1 });
      if (process.platform === 'win32') {
        assert.equal(written.ok, true, written.error);
        assert.deepEqual(JSON.parse(fs.readFileSync(file, 'utf8')), { value: 1 });
      } else {
        assert.equal(written.ok, false);
        assert.match(written.error, /EBUSY/);
      }
    } finally {
      fs.renameSync = rename;
    }
    assert.deepEqual(fs.readdirSync(path.dirname(file)).filter((name) => name.includes('.tmp-')), [], 'no temp file is left either way');
  });
});

test('temp files a killed write left behind are removed, and a write still under way keeps its own', async () => {
  await withHome(async (home) => {
    const state = await fresh('state.mjs');
    const old = new Date(Date.now() - 60 * 60 * 1000);
    const files = {
      record: path.join(home, 'ops', 'abcdefgh-1111-4111-8111-111111111111.json.tmp-4242-0a1b2c3d4e5f'),
      sidecar: path.join(home, 'op.lock.tmp-4242'),
      controller: path.join(home, 'controller.json.tmp-4242-0a1b2c3d'),
      service: path.join(home, 'state', 'service-demo.json.tmp-4242-0a1b2c3d4e5f'),
      fresh: path.join(home, 'state', 'cache.json.tmp-4343-0a1b2c3d4e5f'),
      unrelated: path.join(home, 'state', 'notes.tmp'),
    };
    for (const [name, file] of Object.entries(files)) {
      fs.mkdirSync(path.dirname(file), { recursive: true });
      fs.writeFileSync(file, '{');
      if (name !== 'fresh') fs.utimesSync(file, old, old);
    }
    const removed = state.removeOrphanedTemporaries();
    assert.deepEqual(removed.sort(), [files.record, files.sidecar, files.controller, files.service].sort());
    assert.equal(fs.existsSync(files.fresh), true);
    assert.equal(fs.existsSync(files.unrelated), true, 'only the names an atomic write uses');
  });
});

test('a pre-3.0 state.json is split into the new layout once', async () => {
  await withHome(async (home) => {
    fs.writeFileSync(
      path.join(home, 'state.json'),
      JSON.stringify({
        lastUpdate: { at: '2026-01-01T00:00:00.000Z', result: 'ok' },
        pendingVersion: '9.9.9',
        services: { other: { pendingRestart: true } },
        channelCache: { 'npm:t3:nightly': { version: '1.2.3', at: '2026-01-01T00:00:00.000Z' } },
      }),
    );
    const state = await fresh('state.mjs');

    assert.equal(state.serviceState({ id: 'other' }).pendingRestart, true);
    // The legacy top-level keys belonged to whichever service was first.
    assert.equal(state.serviceState({ id: 'first' }, { isFirst: true }).pendingVersion, '9.9.9');
    assert.equal(state.loadCache().channelCache['npm:t3:nightly'].version, '1.2.3');
    // The old file is left alone rather than deleted, so a rollback to 2.x still
    // finds its history.
    assert.equal(fs.existsSync(path.join(home, 'state.json')), true);
  });
});
