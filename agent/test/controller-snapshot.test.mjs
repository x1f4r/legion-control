import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import {
  canonicalBytes, canonicalHash, controllerMetaPath, controllerPath,
  readController, readControllerMeta, storeController,
} from '../src/controller.mjs';
import { SRC, cli, withHome, writeConfig } from './helpers.mjs';

const document = (revision, lineage = []) => JSON.stringify({
  version: 3, controller: { id: 'snapshot-test', revision, lineage, source: 'cli' },
  machines: [{ id: 'machine', name: `Revision ${revision}` }],
});
const journalPath = (home) => path.join(home, 'controller.commit.json');
const stagedPath = () => `${controllerPath()}.staged`;

function nextRevision(home) {
  const first = storeController(document(1));
  assert.equal(first.ok, true);
  const raw = canonicalBytes(document(2, [first.hash]));
  const meta = { id: 'snapshot-test', revision: 2, updatedAt: '2026-09-05T00:00:00.000Z', source: 'cli',
    hash: canonicalHash(raw), lineage: [first.hash], device: null, bytes: raw.length };
  fs.writeFileSync(stagedPath(), raw);
  return { first, raw, meta, journal: journalPath(home) };
}

function inventory(home) {
  return fs.readdirSync(home).sort().map((name) => {
    const file = path.join(home, name);
    const stat = fs.statSync(file);
    return [name, stat.size, stat.mtimeMs, stat.isFile() ? fs.readFileSync(file).toString('hex') : null];
  });
}

for (const phase of ['staged', 'journal', 'document', 'metadata']) {
  test(`controller readers preserve files and return one coherent snapshot at commit phase ${phase}`, async () => {
    await withHome(async (home) => {
      writeConfig(home, { services: [], updates: { automatic: false }, system: { id: 'test', name: 'Test' } });
      const { first, raw, meta, journal } = nextRevision(home);
      if (phase !== 'staged') fs.writeFileSync(journal, JSON.stringify({ meta }));
      if (['document', 'metadata'].includes(phase)) fs.renameSync(stagedPath(), controllerPath());
      if (phase === 'metadata') fs.writeFileSync(controllerMetaPath(), JSON.stringify(meta));
      const before = inventory(home);
      const expected = phase === 'staged' ? first.hash : meta.hash;
      const snapshot = readController();
      assert.equal(snapshot.consistent, true);
      assert.equal(snapshot.hash, expected);
      assert.equal(snapshot.hash, snapshot.meta.hash);
      assert.equal(canonicalHash(snapshot.raw), expected);
      assert.equal(snapshot.document.controller.revision, snapshot.meta.revision);
      assert.equal(readControllerMeta().hash, expected);
      for (const args of [['config'], ['config', 'meta'], ['status']]) {
        const reply = cli(args, { home }).payload;
        assert.equal(reply.ok, true, reply.error);
        const hash = args[0] === 'status' ? reply.controller.hash : reply.hash;
        assert.equal(hash, expected);
      }
      assert.deepEqual(inventory(home), before, 'read-only CLI calls must not create locks, rename files, or update the journal');
      if (phase !== 'staged') {
        assert.equal(storeController(raw).action, 'noop', 'a mutation may finish the committed transaction before replaying its result');
        assert.equal(fs.existsSync(journal), false);
        assert.equal(fs.existsSync(stagedPath()), false);
        assert.equal(JSON.parse(fs.readFileSync(controllerMetaPath(), 'utf8')).hash, meta.hash);
      }
    });
  });
}

test('a journal with no matching document fails closed without replacing live bytes or metadata', async () => {
  await withHome(async (home) => {
    const { meta, journal } = nextRevision(home);
    fs.writeFileSync(stagedPath(), canonicalBytes(document(99)));
    fs.writeFileSync(journal, JSON.stringify({ meta }));
    const before = inventory(home);
    const snapshot = readController();
    assert.equal(snapshot.consistent, false);
    assert.equal(snapshot.document, null);
    assert.equal(snapshot.meta.hash, null);
    assert.match(snapshot.error, /no matching/);
    assert.throws(() => readControllerMeta(), /no matching/);
    const refused = storeController(document(3, [meta.hash]));
    assert.equal(refused.ok, false);
    assert.deepEqual(inventory(home), before);
  });
});

test('readers reject metadata that matches document bytes but claims a different revision or lineage', async () => {
  await withHome(async (home) => {
    const { meta, journal } = nextRevision(home);
    for (const changed of [{ ...meta, revision: 99 }, { ...meta, lineage: [] }, { ...meta, bytes: meta.bytes + 1 }]) {
      fs.writeFileSync(journal, JSON.stringify({ meta: changed }));
      const before = inventory(home);
      const snapshot = readController();
      assert.equal(snapshot.consistent, false);
      assert.equal(snapshot.document, null);
      assert.deepEqual(inventory(home), before);
    }
  });
});

test('a stable mismatched live pair is reported, never exposed as a matching document and revision', async () => {
  await withHome(async (home) => {
    const first = storeController(document(1));
    fs.writeFileSync(controllerPath(), canonicalBytes(document(2, [first.hash])));
    const before = inventory(home);
    const snapshot = readController();
    assert.equal(snapshot.consistent, false);
    assert.equal(snapshot.document, null);
    assert.equal(snapshot.hash, canonicalHash(fs.readFileSync(controllerPath())));
    assert.equal(snapshot.meta.hash, null);
    assert.match(snapshot.error, /same canonical bytes/);
    assert.deepEqual(inventory(home), before);
  });
});

test('legacy malformed documents still report their actual hash without acquiring identity', async () => {
  await withHome(async (home) => {
    for (const raw of ['{broken', '[]']) {
      fs.writeFileSync(controllerPath(), raw);
      const before = inventory(home);
      const snapshot = readController();
      assert.equal(snapshot.document, null);
      assert.equal(snapshot.hash, canonicalHash(raw));
      assert.equal(snapshot.raw, raw);
      assert.equal(snapshot.meta.id, null);
      assert.ok(snapshot.error);
      assert.deepEqual(inventory(home), before);
    }
    assert.equal(storeController(document(1)).ok, true, 'an unowned malformed legacy copy can still be repaired by a valid push');
  });
});

test('a failed metadata rename retains the committed journal for read-only access and later mutation recovery', async () => {
  await withHome(async (home) => {
    const first = storeController(document(1));
    const next = document(2, [first.hash]);
    const rename = fs.renameSync;
    try {
      fs.renameSync = (source, target) => {
        if (target === controllerMetaPath()) throw new Error('injected metadata rename failure');
        return rename(source, target);
      };
      const failed = storeController(next);
      assert.equal(failed.ok, false);
      assert.match(failed.error, /metadata rename failure/);
    } finally { fs.renameSync = rename; }
    assert.equal(fs.existsSync(journalPath(home)), true);
    const before = inventory(home);
    const snapshot = readController();
    assert.equal(snapshot.hash, canonicalHash(next));
    assert.equal(snapshot.meta.revision, 2);
    assert.deepEqual(inventory(home), before);
    assert.equal(storeController(next).action, 'noop');
    assert.equal(fs.existsSync(journalPath(home)), false);
  });
});

test('a publication between live document and metadata reads triggers a bounded retry', async () => {
  await withHome(async (home) => {
    const first = storeController(document(1));
    const next = document(2, [first.hash]);
    const read = fs.readFileSync;
    let injected = false;
    try {
      fs.readFileSync = (file, ...args) => {
        const bytes = read(file, ...args);
        if (file === controllerPath() && !injected) {
          injected = true;
          assert.equal(storeController(next).ok, true);
        }
        return bytes;
      };
      const snapshot = readController();
      assert.equal(snapshot.consistent, true);
      assert.equal(snapshot.meta.revision, 2);
      assert.equal(snapshot.hash, canonicalHash(next));
    } finally { fs.readFileSync = read; }
  });
});

test('concurrent publication and reads never return mixed documents, hashes, or metadata', async () => {
  await withHome(async (home) => {
    storeController(document(1));
    const done = path.join(home, 'writer.done');
    const source = `import fs from 'node:fs';import {storeController,readControllerMeta} from ${JSON.stringify(pathToFileURL(path.join(SRC, 'controller.mjs')).href)};import {acquireOperationLock} from ${JSON.stringify(pathToFileURL(path.join(SRC, 'lock.mjs')).href)};for(let revision=2;revision<=35;revision++){const held=acquireOperationLock({kind:'config',waitMs:5000});if(!held.ok)throw new Error(held.message);try{const meta=readControllerMeta();const raw=JSON.stringify({version:3,controller:{id:'snapshot-test',revision,lineage:[meta.hash,...meta.lineage].slice(0,32),source:'cli'},machines:[{id:'machine',name:'Revision '+revision}]});const result=storeController(raw);if(!result.ok)throw new Error(result.error);}finally{held.lock.release();}}fs.writeFileSync(${JSON.stringify(done)},'done');`;
    const writer = spawn(process.execPath, ['--input-type=module', '-e', source], { env: { ...process.env, LEGIONCTL_HOME: home }, stdio: ['ignore', 'ignore', 'pipe'] });
    let error = '';
    writer.stderr.on('data', (chunk) => { error += chunk; });
    const completion = new Promise((resolve, reject) => { writer.once('error', reject); writer.once('exit', (code) => code === 0 ? resolve() : reject(new Error(error))); });
    let samples = 0;
    const deadline = Date.now() + 10000;
    try {
      while (!fs.existsSync(done) && Date.now() < deadline) {
        const snapshot = readController();
        if (snapshot.document) {
          assert.equal(snapshot.hash, snapshot.meta.hash);
          assert.equal(canonicalHash(snapshot.raw), snapshot.hash);
          assert.equal(snapshot.document.controller.revision, snapshot.meta.revision);
          assert.deepEqual(snapshot.document.controller.lineage, snapshot.meta.lineage);
        } else assert.ok(snapshot.error);
        samples += 1;
        if (samples % 10 === 0) await new Promise((resolve) => setTimeout(resolve, 1));
      }
      assert.ok(fs.existsSync(done), 'writer must finish within the test deadline');
      await completion;
    } finally { if (writer.exitCode === null) writer.kill('SIGKILL'); }
    assert.ok(samples > 10);
    assert.equal(readControllerMeta().revision, 35);
  });
});
