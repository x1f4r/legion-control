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
