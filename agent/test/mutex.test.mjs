// The cross-process mutex, tested against the failures that forced it to exist.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { once } from 'node:events';
import test from 'node:test';
import { spawn, spawnSync } from 'node:child_process';
import { acquireMutex, mutexAvailable, mutexPath, probeMutex } from '../src/mutex.mjs';
import { SRC, withHome } from './helpers.mjs';

const isWindows = process.platform === 'win32';

/** A child that takes the mutex, says so, and then waits to be killed. */
function holdInChild(db) {
  const source = `
    import { acquireMutex } from ${JSON.stringify(pathToFileURL(path.join(SRC, 'mutex.mjs')).href)};
    const held = acquireMutex(${JSON.stringify(db)}, { waitMs: 2000 });
    process.stdout.write(held.ok ? 'held\\n' : 'failed: ' + held.error + '\\n');
    if (!held.ok) process.exit(1);
    setInterval(() => {}, 1000);
  `;
  const child = spawn(process.execPath, ['--input-type=module', '-e', source], { stdio: ['ignore', 'pipe', 'pipe'] });
  return new Promise((resolve, reject) => {
    let out = '';
    child.stdout.on('data', (chunk) => {
      out += chunk;
      if (out.includes('held')) resolve(child);
      if (out.includes('failed')) reject(new Error(out));
    });
    child.on('exit', () => reject(new Error(`the holder exited early: ${out}`)));
    setTimeout(() => reject(new Error('the holder never reported')), 10000);
  });
}

test('node:sqlite is available, so the agent can lock at all', () => {
  assert.equal(mutexAvailable(), true, 'this Node cannot provide the agent mutex');
});

test('a second acquisition in the same process is refused', async () => {
  await withHome(async (home) => {
    const db = mutexPath(path.join(home, 'a.lock'));
    const first = acquireMutex(db, { waitMs: 0 });
    assert.equal(first.ok, true);
    const second = acquireMutex(db, { waitMs: 0 });
    assert.equal(second.ok, false);
    assert.equal(second.busy, true);
    first.release();
    const third = acquireMutex(db, { waitMs: 0 });
    assert.equal(third.ok, true);
    third.release();
  });
});

test('two different lock databases nest freely', async () => {
  await withHome(async (home) => {
    const outer = acquireMutex(mutexPath(path.join(home, 'op.lock')), { waitMs: 0 });
    const inner = acquireMutex(mutexPath(path.join(home, 'state', '.lock')), { waitMs: 0 });
    assert.equal(outer.ok, true);
    assert.equal(inner.ok, true, 'the state lock must be takeable while the operation lock is held');
    inner.release();
    outer.release();
  });
});

test('release is idempotent', async () => {
  await withHome(async (home) => {
    const held = acquireMutex(mutexPath(path.join(home, 'a.lock')), { waitMs: 0 });
    held.release();
    held.release();
    const again = acquireMutex(mutexPath(path.join(home, 'a.lock')), { waitMs: 0 });
    assert.equal(again.ok, true);
    again.release();
  });
});

test('the death of the holder releases the lock', async () => {
  await withHome(async (home) => {
    const db = mutexPath(path.join(home, 'op.lock'));
    const child = await holdInChild(db);
    try {
      assert.equal(acquireMutex(db, { waitMs: 0 }).busy, true, 'the child should hold it');
      child.kill('SIGKILL');
      // No pid is inspected and no file is reclaimed: the kernel drops the lock.
      const started = Date.now();
      let taken = null;
      while (Date.now() - started < 5000) {
        taken = acquireMutex(db, { waitMs: 100 });
        if (taken.ok) break;
      }
      assert.equal(taken.ok, true, 'the lock was not released when its holder was killed');
      taken.release();
    } finally {
      if (child.exitCode === null && child.signalCode === null) {
        const exited = once(child, 'exit');
        child.kill('SIGKILL');
        await exited;
      }
    }
  });
});

test('the wait is bounded, and zero returns at once', async () => {
  await withHome(async (home) => {
    const db = mutexPath(path.join(home, 'op.lock'));
    const child = await holdInChild(db);
    try {
      const immediate = Date.now();
      const first = acquireMutex(db, { waitMs: 0 });
      const immediateMs = Date.now() - immediate;
      assert.equal(first.busy, true);
      assert.ok(immediateMs < 200, `waitMs 0 took ${immediateMs} ms`);

      const started = Date.now();
      const second = acquireMutex(db, { waitMs: 300 });
      const elapsed = Date.now() - started;
      assert.equal(second.busy, true);
      assert.ok(elapsed >= 200, `waitMs 300 returned after only ${elapsed} ms`);
      assert.ok(elapsed < 3000, `waitMs 300 took ${elapsed} ms`);
    } finally {
      if (child.exitCode === null && child.signalCode === null) {
        const exited = once(child, 'exit');
        child.kill('SIGKILL');
        await exited;
      }
    }
  });
});

test('probing does not create the lock database', async () => {
  await withHome(async (home) => {
    const db = mutexPath(path.join(home, 'never-used.lock'));
    assert.deepEqual(probeMutex(db), { state: 'free', error: null });
    assert.equal(fs.existsSync(db), false, 'a read-only probe must leave no trace');
    assert.equal(fs.existsSync(path.dirname(db)), true);
  });
});

test('probing reports a held lock without disturbing it', async () => {
  await withHome(async (home) => {
    const db = mutexPath(path.join(home, 'op.lock'));
    const child = await holdInChild(db);
    try {
      assert.equal(probeMutex(db).state, 'held');
      assert.equal(probeMutex(db).state, 'held', 'probing twice must not release anything');
    } finally {
      if (child.exitCode === null && child.signalCode === null) {
        const exited = once(child, 'exit');
        child.kill('SIGKILL');
        await exited;
      }
    }
  });
});

test('a file that is not a database is refused with a usable hint', async () => {
  await withHome(async (home) => {
    const db = mutexPath(path.join(home, 'op.lock'));
    fs.mkdirSync(path.dirname(db), { recursive: true });
    fs.writeFileSync(db, 'this is not a database, it is a note somebody left');
    const taken = acquireMutex(db, { waitMs: 0 });
    assert.equal(taken.ok, false);
    assert.equal(taken.busy, false, 'a corrupt database is not "somebody else holds it"');
    assert.match(`${taken.error} ${taken.hint}`, /not a lock database|could not be opened/);
  });
});

test('a read-only database is refused rather than silently giving no exclusion', { skip: isWindows || process.getuid?.() === 0 }, async () => {
  await withHome(async (home) => {
    const db = mutexPath(path.join(home, 'op.lock'));
    const first = acquireMutex(db, { waitMs: 0 });
    first.release();
    fs.chmodSync(db, 0o444);
    try {
      const taken = acquireMutex(db, { waitMs: 0 });
      assert.equal(taken.ok, false);
      assert.equal(taken.busy, false);
      assert.match(taken.error, /not writable/);
      assert.match(taken.hint ?? '', /sudo|ownership/);
    } finally {
      fs.chmodSync(db, 0o644);
    }
  });
});

test('an unwritable directory is refused with a usable hint', { skip: isWindows || process.getuid?.() === 0 }, async () => {
  await withHome(async (home) => {
    const dir = path.join(home, 'readonly');
    fs.mkdirSync(dir);
    fs.chmodSync(dir, 0o500);
    try {
      const taken = acquireMutex(path.join(dir, 'op.lock.sqlite'), { waitMs: 0 });
      assert.equal(taken.ok, false);
      assert.equal(taken.busy, false);
      assert.match(taken.error, /not writable|could not be/);
    } finally {
      fs.chmodSync(dir, 0o700);
    }
  });
});

test('no source file opens a lock database through the ordinary file API', () => {
  // Reading the database with fs inside the holding process silently DROPS the
  // lock, so this is a rule about the source rather than about behaviour.
  const offenders = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
        continue;
      }
      if (!entry.name.endsWith('.mjs')) continue;
      const text = fs.readFileSync(full, 'utf8');
      for (const [index, line] of text.split('\n').entries()) {
        // Prose about the rule is not a breach of it.
        if (/^\s*(\/\/|\*|\/\*)/.test(line)) continue;
        if (!/mutexPath|\.sqlite/.test(line)) continue;
        if (/readFileSync|createReadStream|openSync|rmSync|unlinkSync/.test(line)) {
          offenders.push(`${path.relative(SRC, full)}:${index + 1}: ${line.trim()}`);
        }
      }
    }
  };
  walk(SRC);
  assert.deepEqual(offenders, [], 'a lock database must only ever be opened by SQLite, and never deleted');
});

test('the counter race the file protocol lost is now exact', async () => {
  await withHome(async (home) => {
    const store = path.join(home, 'counter.json');
    const source = `
      import { withStore } from ${JSON.stringify(pathToFileURL(path.join(SRC, 'state.mjs')).href)};
      for (let n = 0; n < 15; n += 1) {
        const result = withStore(${JSON.stringify(store)}, (current) => {
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5);
          return { count: (current.count ?? 0) + 1 };
        }, { waitMs: 20000 });
        if (!result.ok) { console.error(JSON.stringify(result)); process.exit(1); }
      }
    `;
    // A leftover note from a long-dead process, which the old protocol treated as
    // an invitation to reclaim by unlinking.
    fs.mkdirSync(path.join(home, 'state'), { recursive: true });
    fs.writeFileSync(path.join(home, 'state', '.lock'), 'pid=99999999\nnonce=aaaaaaaaaaaaaaaa\n');

    const children = Array.from(
      { length: 8 },
      () =>
        new Promise((resolve, reject) => {
          const child = spawn(process.execPath, ['--input-type=module', '-e', source], {
            env: { ...process.env, LEGIONCTL_HOME: home },
            stdio: ['ignore', 'ignore', 'pipe'],
          });
          let err = '';
          child.stderr.on('data', (chunk) => {
            err += chunk;
          });
          child.on('exit', (code) => (code === 0 ? resolve() : reject(new Error(err))));
        }),
    );
    await Promise.all(children);
    assert.equal(JSON.parse(fs.readFileSync(store, 'utf8')).count, 120);
  });
});

test('a mutating command refuses cleanly when node:sqlite is missing', async () => {
  await withHome(async (home) => {
    fs.writeFileSync(
      path.join(home, 'config.json'),
      JSON.stringify({ configVersion: 3, updates: { automatic: true }, services: [], boot: { targets: {} } }),
    );
    // --no-experimental-sqlite is how a Node without the binding behaves.
    const result = spawnSync(
      process.execPath,
      ['--no-experimental-sqlite', path.join(SRC, 'index.mjs'), 'version'],
      { encoding: 'utf8', env: { ...process.env, LEGIONCTL_HOME: home } },
    );
    // Whatever this Node makes of the flag, `version` must answer with one JSON
    // object rather than crashing at import: a machine that cannot say what it is
    // running cannot be diagnosed remotely.
    const payload = JSON.parse(result.stdout);
    assert.equal(payload.ok, true);
    assert.equal(payload.agentVersion, '3.0.1');
  });
});
