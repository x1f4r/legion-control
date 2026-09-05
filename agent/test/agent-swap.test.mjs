import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { spawnSync } from 'node:child_process';
import { commitInstallSwap, commitRollbackSwap, recoverAgentSwap, swapPaths, verifyLocalBaseline } from '../src/agent-swap.mjs';

function home() { return fs.mkdtempSync(path.join(os.tmpdir(), 'legion-swap-')); }
function tree(dir, value) { fs.mkdirSync(dir, { recursive: true }); fs.writeFileSync(path.join(dir, 'value'), value); }
function value(dir) { return fs.readFileSync(path.join(dir, 'value'), 'utf8'); }

function failRename(sourceName, destinationName, { occurrence = 1 } = {}) {
  let seen = 0;
  return new Proxy(fs, {
    get(target, property) {
      if (property !== 'renameSync') return Reflect.get(target, property);
      return (source, destination) => {
        if (path.basename(source) === sourceName && path.basename(destination) === destinationName && ++seen === occurrence) {
          const error = new Error(`fault ${sourceName}->${destinationName}`);
          error.code = 'EIO';
          throw error;
        }
        return fs.renameSync(source, destination);
      };
    },
  });
}

test('install faults before commit restore every original tree', () => {
  for (const [source, destination] of [
    ['agent.prev', 'agent.swap-old-prev'],
    ['agent', 'agent.prev'],
    ['agent.new', 'agent'],
  ]) {
    const base = home();
    try {
      const p = swapPaths(base);
      tree(p.live, 'live'); tree(p.previous, 'previous'); tree(p.staging, 'new');
      const result = commitInstallSwap(base, { io: failRename(source, destination) });
      assert.equal(result.ok, false, `${source}->${destination}`);
      assert.equal(value(p.live), 'live');
      assert.equal(value(p.previous), 'previous');
      assert.equal(value(p.staging), 'new');
      assert.equal(fs.existsSync(p.oldPrevious), false);
      assert.equal(fs.existsSync(p.journal), false);
    } finally { fs.rmSync(base, { recursive: true, force: true }); }
  }
});

test('a lost journal write after the install rename reports the committed install', () => {
  const base = home();
  try {
    const p = swapPaths(base);
    tree(p.live, 'live'); tree(p.previous, 'previous'); tree(p.staging, 'new');
    let journalWrites = 0;
    const io = new Proxy(fs, {
      get(target, property) {
        if (property !== 'renameSync') return Reflect.get(target, property);
        return (source, destination) => {
          if (destination === p.journal && ++journalWrites === 4) throw new Error('journal fault after commit');
          return fs.renameSync(source, destination);
        };
      },
    });
    const result = commitInstallSwap(base, { io });
    assert.equal(result.ok, true);
    assert.equal(result.committed, true);
    assert.equal(value(p.live), 'new');
    assert.equal(value(p.previous), 'live');
    assert.match(result.notes.join(' '), /committed/);
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('rollback faults before commit restore the live tree', () => {
  for (const [source, destination] of [
    ['agent', 'agent.rollback'],
    ['agent.rollback.new', 'agent'],
  ]) {
    const base = home();
    try {
      const p = swapPaths(base);
      tree(p.live, 'live'); tree(p.previous, 'previous'); tree(p.rollbackStaging, 'rollback-copy');
      const result = commitRollbackSwap(base, { io: failRename(source, destination) });
      assert.equal(result.ok, false, `${source}->${destination}`);
      assert.equal(value(p.live), 'live');
      assert.equal(value(p.previous), 'previous');
      assert.equal(value(p.rollbackStaging), 'rollback-copy');
      assert.equal(fs.existsSync(p.rollbackParked), false);
    } finally { fs.rmSync(base, { recursive: true, force: true }); }
  }
});

test('rollback cleanup faults stay successful and preserve the outgoing tree', () => {
  for (const [source, destination] of [
    ['agent.prev', 'agent.rollback.used'],
    ['agent.rollback', 'agent.prev'],
  ]) {
    const base = home();
    try {
      const p = swapPaths(base);
      tree(p.live, 'live'); tree(p.previous, 'previous'); tree(p.rollbackStaging, 'rollback-copy');
      const result = commitRollbackSwap(base, { io: failRename(source, destination) });
      assert.equal(result.ok, true, `${source}->${destination}`);
      assert.equal(result.committed, true);
      assert.equal(value(p.live), 'rollback-copy');
      assert.equal(value(p.rollbackParked), 'live');
      assert.ok(result.notes.length > 0);

      const recovered = recoverAgentSwap(base);
      assert.equal(recovered.ok, true);
      assert.equal(value(p.previous), 'live');
      assert.equal(fs.existsSync(p.rollbackParked), false);
      assert.equal(fs.existsSync(p.journal), false);
    } finally { fs.rmSync(base, { recursive: true, force: true }); }
  }
});

test('recovery never deletes an unowned parked backup', () => {
  const base = home();
  try {
    const p = swapPaths(base);
    tree(p.live, 'live'); tree(p.rollbackParked, 'valuable');
    const result = recoverAgentSwap(base);
    assert.equal(result.ok, false);
    assert.equal(value(p.rollbackParked), 'valuable');
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('process death after every install and rollback rename recovers an intact layout', () => {
  const moduleUrl = new URL('../src/agent-swap.mjs', import.meta.url).href;
  for (const operation of ['install', 'rollback']) {
    // These include journal publication, directory renames and backup rotation.
    for (let stopAfter = 1; stopAfter <= 8; stopAfter += 1) {
      const base = home();
      try {
        const p = swapPaths(base);
        tree(p.live, 'live'); tree(p.previous, 'previous');
        tree(operation === 'install' ? p.staging : p.rollbackStaging, 'candidate');
        const child = spawnSync(process.execPath, ['--input-type=module', '-e', `
          import fs from 'node:fs';
          import { commitInstallSwap, commitRollbackSwap } from ${JSON.stringify(moduleUrl)};
          let renamed = 0;
          const io = new Proxy(fs, { get(target, property) {
            if (property !== 'renameSync') return Reflect.get(target, property);
            return (source, destination) => {
              fs.renameSync(source, destination);
              if (++renamed === ${stopAfter}) process.exit(77);
            };
          }});
          ${operation === 'install' ? 'commitInstallSwap' : 'commitRollbackSwap'}(process.argv[1], { io });
        `, base], { encoding: 'utf8', timeout: 5_000 });
        assert.equal(child.status, 77, `${operation} rename ${stopAfter}: ${child.stderr}`);
        const recovered = recoverAgentSwap(base);
        assert.equal(recovered.ok, true, `${operation} rename ${stopAfter}: ${recovered.error}`);
        const committed = stopAfter >= (operation === 'install' ? 7 : 5);
        assert.equal(value(p.live), committed ? 'candidate' : 'live');
        assert.equal(value(p.previous), committed ? 'live' : 'previous');
        assert.equal(fs.existsSync(p.journal), false);
        assert.equal(recoverAgentSwap(base).ok, true);
      } finally { fs.rmSync(base, { recursive: true, force: true }); }
    }
  }
});

test('local baseline attests only the complete old live tree and rejects tampering', () => {
  const base = home();
  try {
    const p = swapPaths(base);
    tree(p.live, 'unsigned legacy live'); tree(p.staging, 'new');
    let anchor;
    const io = new Proxy(fs, { get(target, property) {
      if (property !== 'renameSync') return Reflect.get(target, property);
      return (source, destination) => {
        const result = fs.renameSync(source, destination);
        if (source === p.live) {
          anchor = JSON.parse(fs.readFileSync(p.journal, 'utf8')).baselineSha256;
          assert.equal(verifyLocalBaseline(base, p.previous, anchor).ok, true);
          fs.writeFileSync(path.join(p.previous, 'extra'), 'unlisted');
          throw new Error('interrupted after backup tampering');
        }
        return result;
      };
    }});
    const result = commitInstallSwap(base, { io });
    assert.equal(result.ok, false);
    assert.match(result.error, /pre-swap local baseline/);
    assert.equal(fs.existsSync(p.live), false);
    assert.equal(value(p.previous), 'unsigned legacy live');
    assert.equal(value(p.staging), 'new');
    fs.unlinkSync(path.join(p.previous, 'extra'));
    assert.equal(recoverAgentSwap(base).ok, true);
    assert.equal(value(p.live), 'unsigned legacy live');
    fs.appendFileSync(p.baseline, ' ');
    assert.equal(verifyLocalBaseline(base, p.live, anchor).ok, false);
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('an invalid swap journal blocks replacement and preserves all trees', () => {
  for (const journal of ['{', JSON.stringify({ schema: 1, operation: 'install', phase: 'unknown' })]) {
    const base = home();
    try {
      const p = swapPaths(base);
      tree(p.live, 'live'); tree(p.previous, 'previous'); tree(p.staging, 'new');
      fs.writeFileSync(p.journal, journal);
      const recovered = commitInstallSwap(base);
      assert.equal(recovered.ok, false);
      assert.match(recovered.error, /journal.*invalid/);
      assert.equal(value(p.live), 'live');
      assert.equal(value(p.previous), 'previous');
      assert.equal(value(p.staging), 'new');
      assert.equal(fs.readFileSync(p.journal, 'utf8'), journal);
    } finally { fs.rmSync(base, { recursive: true, force: true }); }
  }
});
