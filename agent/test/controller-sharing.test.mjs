import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { canonicalHash, controllerMetaPath, controllerPath, readController, storeController } from '../src/controller.mjs';
import { withHome } from './helpers.mjs';

const document = (revision, lineage = []) => JSON.stringify({ version: 3,
  controller: { id: 'sharing-test', revision, lineage, source: 'cli' }, machines: [] });

for (const phase of ['document', 'metadata', 'journal']) {
  test(`Windows transient sharing conflicts preserve a coherent committed snapshot during ${phase} publication`, async () => withHome(async (home) => {
    const first = storeController(document(1));
    const next = document(2, [first.hash]);
    const oldBytes = fs.readFileSync(controllerPath());
    const rename = fs.renameSync;
    const remove = fs.rmSync;
    const platform = Object.getOwnPropertyDescriptor(process, 'platform');
    let attempts = 0;
    const block = () => {
      attempts += 1;
      if (attempts > 3) return;
      const snapshot = readController();
      assert.equal(snapshot.hash, canonicalHash(next));
      assert.equal(snapshot.meta.hash, snapshot.hash);
      if (phase === 'document') assert.deepEqual(fs.readFileSync(controllerPath()), oldBytes, 'replacement must never delete the old destination');
      const error = new Error('fixture reader temporarily denies replacement'); error.code = 'EPERM'; throw error;
    };
    try {
      Object.defineProperty(process, 'platform', { value: 'win32' });
      fs.renameSync = (source, target) => {
        if (target === (phase === 'document' ? controllerPath() : phase === 'metadata' ? controllerMetaPath() : null)) block();
        return rename(source, target);
      };
      fs.rmSync = (file, options) => {
        if (phase === 'journal' && file === path.join(home, 'controller.commit.json')) block();
        return remove(file, options);
      };
      const result = storeController(next);
      assert.equal(result.ok, true, result.error);
      assert.equal(attempts, 4);
      assert.equal(readController().hash, canonicalHash(next));
    } finally {
      fs.renameSync = rename; fs.rmSync = remove;
      Object.defineProperty(process, 'platform', platform);
    }
  }));
}

test('persistent Windows denial is bounded and retains both live bytes and the committed recovery journal', async () => withHome(async (home) => {
  const first = storeController(document(1));
  const oldBytes = fs.readFileSync(controllerPath());
  const next = document(2, [first.hash]);
  const rename = fs.renameSync;
  const platform = Object.getOwnPropertyDescriptor(process, 'platform');
  let attempts = 0;
  const started = Date.now();
  try {
    Object.defineProperty(process, 'platform', { value: 'win32' });
    fs.renameSync = (source, target) => {
      if (target === controllerPath()) {
        attempts += 1;
        assert.deepEqual(fs.readFileSync(controllerPath()), oldBytes);
        const error = new Error('fixture access denied'); error.code = 'EACCES'; throw error;
      }
      return rename(source, target);
    };
    const result = storeController(next);
    assert.equal(result.ok, false);
    assert.match(result.error, /fixture access denied/);
    assert.ok(attempts > 1);
    assert.ok(Date.now() - started < 1800, 'permanent denial must stop retrying');
    assert.deepEqual(fs.readFileSync(controllerPath()), oldBytes);
    assert.ok(fs.existsSync(path.join(home, 'controller.commit.json')));
    assert.ok(fs.existsSync(`${controllerPath()}.staged`));
    assert.equal(readController().hash, canonicalHash(next), 'the committed snapshot remains readable');
  } finally { fs.renameSync = rename; Object.defineProperty(process, 'platform', platform); }
  assert.equal(storeController(next).action, 'noop', 'the next mutation recovers the committed version');
}));

test('Windows publication waits for an actual reader handle to release delete sharing', { skip: process.platform !== 'win32' }, async () => withHome(async (home) => {
  const first = storeController(document(1));
  const ready = path.join(home, 'reader.ready');
  const quote = (value) => `'${value.replace(/'/g, "''")}'`;
  const script = `$handle=[System.IO.File]::Open(${quote(controllerPath())},[System.IO.FileMode]::Open,[System.IO.FileAccess]::Read,[System.IO.FileShare]::ReadWrite);try{[System.IO.File]::WriteAllText(${quote(ready)},'ready');Start-Sleep -Milliseconds 200}finally{$handle.Dispose()}`;
  const reader = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { windowsHide: true, stdio: 'ignore' });
  const completion = new Promise((resolve, reject) => { reader.once('error', reject); reader.once('exit', resolve); });
  try {
    const deadline = Date.now() + 5000;
    while (!fs.existsSync(ready) && Date.now() < deadline) await new Promise((resolve) => setTimeout(resolve, 5));
    assert.ok(fs.existsSync(ready), 'the reader must hold the actual destination file');
    const next = document(2, [first.hash]);
    const result = storeController(next);
    assert.equal(result.ok, true, result.error);
    assert.equal(readController().hash, canonicalHash(next));
  } finally {
    if (reader.exitCode === null) reader.kill();
    await completion;
  }
}));
