import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { saveConfig, configPath, lastGoodConfigPath } from '../src/config.mjs';
import { acquireFileLock, stateLockPath } from '../src/state.mjs';
import { SRC, withHome, writeConfig } from './helpers.mjs';

async function waitFor(file) {
  const deadline = Date.now() + 5000;
  while (!fs.existsSync(file)) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${file}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

function patchProcess(home, code) {
  const source = `import fs from 'node:fs';import {saveConfig} from ${JSON.stringify(pathToFileURL(path.join(SRC, 'config.mjs')).href)};${code}`;
  const child = spawn(process.execPath, ['--input-type=module', '-e', source], { env: { ...process.env, LEGIONCTL_HOME: home } });
  let stdout = ''; let stderr = '';
  child.stdout.on('data', (chunk) => { stdout += chunk; });
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const done = new Promise((resolve, reject) => {
    child.on('error', reject);
    child.on('close', (status) => {
      try { assert.equal(status, 0, stderr); resolve(JSON.parse(stdout)); } catch (error) { reject(error); }
    });
  });
  return { child, done };
}

test('concurrent disjoint configuration patches read the latest committed document under the state mutex', async () => {
  await withHome(async (home) => {
    const original = { configVersion: 3, services: [], updates: { automatic: true, maintenanceWindows: [] }, system: { id: 'test', name: 'Test machine' } };
    writeConfig(home, original);
    const entered = path.join(home, 'first-entered');
    const release = path.join(home, 'release-first');
    const secondReady = path.join(home, 'second-ready');
    const first = patchProcess(home, `console.log(JSON.stringify(saveConfig(stored=>{fs.writeFileSync(${JSON.stringify(entered)},'1');const deadline=Date.now()+5000;while(!fs.existsSync(${JSON.stringify(release)})&&Date.now()<deadline)Atomics.wait(new Int32Array(new SharedArrayBuffer(4)),0,0,5);return {...stored,updates:{...stored.updates,automatic:false}};})));`);
    let second;
    try {
      await waitFor(entered);
      second = patchProcess(home, `fs.writeFileSync(${JSON.stringify(secondReady)},'1');console.log(JSON.stringify(saveConfig(stored=>({...stored,updates:{...stored.updates,pauseUntil:'2099-01-01T00:00:00.000Z'}}))));`);
      await waitFor(secondReady);
      await new Promise((resolve) => setTimeout(resolve, 150));
    } finally { fs.writeFileSync(release, '1'); }
    assert.equal((await first.done).ok, true);
    assert.equal((await second.done).ok, true);
    const saved = JSON.parse(fs.readFileSync(configPath(), 'utf8'));
    assert.equal(saved.updates.automatic, false);
    assert.equal(saved.updates.pauseUntil, '2099-01-01T00:00:00.000Z');
    assert.deepEqual(saved.system, original.system);
    assert.equal(fs.readFileSync(lastGoodConfigPath(), 'utf8'), fs.readFileSync(configPath(), 'utf8'));
  });
});

test('configuration contention refuses without executing a stale callback', async () => {
  await withHome(async (home) => {
    writeConfig(home, { services: [], updates: { automatic: false } });
    const before = fs.readFileSync(configPath(), 'utf8');
    const held = acquireFileLock(stateLockPath());
    let called = false;
    try {
      const result = saveConfig((stored) => { called = true; return stored; }, { waitMs: 50 });
      assert.equal(result.ok, false);
      assert.match(result.error, /did not free/);
      assert.equal(called, false);
      assert.equal(fs.readFileSync(configPath(), 'utf8'), before);
    } finally { held.release(); }
    assert.equal(saveConfig({ updates: { automatic: true } }).ok, true);
  });
});

test('callback and validation failures leave config untouched and release the state mutex', async () => {
  await withHome(async (home) => {
    writeConfig(home, { services: [], updates: { automatic: false } });
    const before = fs.readFileSync(configPath(), 'utf8');
    assert.equal(saveConfig(() => { throw new Error('expected document hash changed'); }).ok, false);
    assert.equal(saveConfig({ updates: { automatic: 'invalid' } }).ok, false);
    assert.equal(fs.readFileSync(configPath(), 'utf8'), before);
    assert.equal(saveConfig({ updates: { automatic: true } }).ok, true);
  });
});

test('partial config patches do not stamp normalized defaults into raw documents', async () => {
  await withHome(async (home) => {
    const stored = { services: [], updates: { automatic: false }, boot: { targets: {} } };
    writeConfig(home, stored);
    assert.equal(saveConfig((current) => ({ ...current, updates: { ...current.updates, pauseUntil: '2099-01-01T00:00:00.000Z' } })).ok, true);
    const written = JSON.parse(fs.readFileSync(configPath(), 'utf8'));
    assert.deepEqual(written, { ...stored, updates: { ...stored.updates, pauseUntil: '2099-01-01T00:00:00.000Z' } });
  });
});

test('unknown and malformed existing fields are retained on validation refusal', async () => {
  await withHome(async (home) => {
    for (const raw of ['', '{broken', JSON.stringify({ services: [], futureSetting: { retain: [1, 'two'] } })]) {
      fs.writeFileSync(configPath(), raw);
      assert.equal(saveConfig({ updates: { automatic: false } }).ok, false);
      assert.equal(fs.readFileSync(configPath(), 'utf8'), raw);
    }
  });
});
