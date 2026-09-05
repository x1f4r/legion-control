import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { runCommandAsync } from '../src/config.mjs';
import { SRC, withHome } from './helpers.mjs';

const posix = process.platform !== 'win32';

test('async deadline kills descendants that retain stdout after their parent exits', { skip: !posix }, async () => {
  await withHome(async (home) => {
    const marker = path.join(home, 'descendant-ran');
    const descendant = `setTimeout(()=>{require('node:fs').writeFileSync(${JSON.stringify(marker)},'unwanted work');},900);`;
    const parent = `require('node:child_process').spawn(process.execPath,['-e',${JSON.stringify(descendant)}],{stdio:['ignore',1,2]}).unref();process.stdout.write(String(process.pid));`;
    const started = Date.now();
    const result = await runCommandAsync(process.execPath, ['-e', parent], { timeoutMs: 180 });
    assert.equal(result.timedOut, true);
    assert.equal(result.ok, false);
    assert.match(result.error, /outcome is unknown/);
    assert.ok(Date.now() - started < 650, 'inherited pipes must not extend the deadline');
    await new Promise((resolve) => setTimeout(resolve, 1000));
    assert.equal(fs.existsSync(marker), false, 'the timed-out descendant must not perform its later mutation');
  });
});

test('an isolated CLI exits at its deadline when a descendant inherited its pipes', { skip: !posix }, async () => {
  const descendant = 'setTimeout(()=>{},5000)';
  const parent = `require('node:child_process').spawn(process.execPath,['-e',${JSON.stringify(descendant)}],{stdio:['ignore',1,2]}).unref();`;
  const source = `import {runCommandAsync} from ${JSON.stringify(pathToFileURL(path.join(SRC, 'config.mjs')).href)};console.log(JSON.stringify(await runCommandAsync(process.execPath,['-e',${JSON.stringify(parent)}],{timeoutMs:150})));`;
  const started = Date.now();
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', source], { encoding: 'utf8', timeout: 2000 });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).timedOut, true);
  assert.ok(Date.now() - started < 1000);
});

test('failed process-group cancellation still settles and closes inherited pipes', { skip: !posix }, async () => {
  const originalKill = process.kill;
  const descendant = 'setTimeout(()=>{},5000)';
  const parent = `require('node:child_process').spawn(process.execPath,['-e',${JSON.stringify(descendant)}],{stdio:['ignore',1,2]}).unref();process.stdout.write(String(process.pid));`;
  let group;
  try {
    process.kill = () => { const error = new Error('injected cancellation failure'); error.code = 'EPERM'; throw error; };
    const started = Date.now();
    const result = await runCommandAsync(process.execPath, ['-e', parent], { timeoutMs: 180 });
    group = Number(result.stdout);
    assert.equal(result.ok, false);
    assert.equal(result.timedOut, true);
    assert.match(result.error, /termination could not be confirmed/);
    assert.ok(Date.now() - started < 650);
  } finally {
    process.kill = originalKill;
    if (group) {
      try { process.kill(-group, 'SIGKILL'); } catch { /* the group already exited */ }
    }
  }
});

test('successful asynchronous commands retain their result and bounded output', async () => {
  const result = await runCommandAsync(process.execPath, ['-e', "process.stdout.write('abcdef');process.stderr.write('ghijkl');"], { timeoutMs: 1000, maxBuffer: 3 });
  assert.equal(result.ok, true);
  assert.equal(result.code, 0);
  assert.equal(result.timedOut, false);
  assert.equal(result.stdout, 'abc');
  assert.equal(result.stderr, 'ghi');
});

test('Windows disposable SQLite worker has exited and released its database when timeout resolves', { skip: posix }, async () => {
  await withHome(async (home) => {
    const database = path.join(home, 'worker.sqlite');
    const script = `const {DatabaseSync}=require('node:sqlite');const db=new DatabaseSync(process.argv[1]);
      db.exec('CREATE TABLE fixture(value)');process.stdout.write(String(process.pid));
      db.prepare('WITH RECURSIVE counts(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM counts WHERE n < 1000000000) SELECT sum(n) FROM counts').get();`;
    const started = Date.now();
    const result = await runCommandAsync(process.execPath, ['-e', script, database], { timeoutMs: 1500, terminateTree: false });
    assert.equal(result.timedOut, true);
    assert.equal(result.terminationConfirmed, true, result.error);
    const pid = Number(result.stdout);
    assert.ok(pid > 0, 'the worker must have opened SQLite before its deadline');
    assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' }, 'the owned worker must already have exited');
    fs.rmSync(database);
    assert.equal(fs.existsSync(database), false, 'database cleanup must work immediately without retrying around a live process');
    assert.ok(Date.now() - started < 2300, 'exit confirmation remains bounded');
  });
});

test('Windows tree timeout lets taskkill finish stopping descendants', { skip: posix }, async () => {
  await withHome(async (home) => {
    const marker = path.join(home, 'descendant-ran');
    const descendant = `setTimeout(()=>require('node:fs').writeFileSync(${JSON.stringify(marker)},'unwanted work'),1800)`;
    const parent = `require('node:child_process').spawn(process.execPath,['-e',${JSON.stringify(descendant)}],{stdio:'ignore'});setInterval(()=>{},1000)`;
    const result = await runCommandAsync(process.execPath, ['-e', parent], { timeoutMs: 400 });
    assert.equal(result.timedOut, true);
    await new Promise((resolve) => setTimeout(resolve, 2200));
    assert.equal(fs.existsSync(marker), false, 'tree cleanup must finish instead of being killed after 250 ms');
  });
});
