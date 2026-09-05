import assert from 'node:assert/strict';
import childProcess from 'node:child_process';
import { EventEmitter } from 'node:events';
import fs from 'node:fs';
import { syncBuiltinESMExports } from 'node:module';
import path from 'node:path';
import { PassThrough } from 'node:stream';
import test from 'node:test';
import { deadline } from '../src/config.mjs';
import { statusBusy } from '../src/status-probes.mjs';

test('unconfirmed Windows worker exit returns unknown and defers scratch cleanup until exit', async () => {
  const originalSpawn = childProcess.spawn;
  const originalRemove = fs.rmSync;
  const platform = Object.getOwnPropertyDescriptor(process, 'platform');
  const child = new EventEmitter();
  Object.assign(child, { pid: 12345, exitCode: null, signalCode: null, killed: false,
    stdin: new PassThrough(), stdout: new PassThrough(), stderr: new PassThrough(),
    kill: () => false, unref: () => {} });
  let scratch;
  let alive = true;
  let prematureRemoval = 0;
  try {
    Object.defineProperty(process, 'platform', { value: 'win32' });
    childProcess.spawn = (_file, _args, options) => {
      scratch = options.env.TEMP;
      fs.writeFileSync(path.join(scratch, 'held.sqlite'), 'fixture');
      return child;
    };
    syncBuiltinESMExports();
    fs.rmSync = (file, options) => {
      if (file === scratch && alive) {
        prematureRemoval += 1;
        const error = new Error('fixture worker still owns the database'); error.code = 'EPERM'; throw error;
      }
      return originalRemove(file, options);
    };
    const result = await statusBusy({ name: 'Fixture', busy: { type: 't3-sqlite' } }, { clock: deadline(20), liveness: null });
    assert.equal(result.unknown, true);
    assert.equal(result.evidence, 'timed-out');
    assert.match(result.error, /worker exit is unconfirmed/);
    assert.equal(prematureRemoval, 0, 'cleanup must not compete with a worker whose termination failed');
    assert.equal(fs.existsSync(scratch), true);
    alive = false;
    child.emit('exit', 1, null);
    assert.equal(fs.existsSync(scratch), false, 'observing the later exit must finish deferred cleanup');
  } finally {
    Object.defineProperty(process, 'platform', platform);
    childProcess.spawn = originalSpawn;
    syncBuiltinESMExports();
    fs.rmSync = originalRemove;
    if (scratch) originalRemove(scratch, { recursive: true, force: true });
  }
});
