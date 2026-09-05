#!/usr/bin/env node
// Stable Legion Control launcher.
//
// Installers copy this file outside the replaceable agent tree. That lets a
// scheduled run or a restricted SSH command repair the one dangerous crash
// gap in a self-update: the old tree has been parked but the new tree has not
// yet reached the live name. It never imports code from a candidate tree while
// deciding what to recover.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const PUBLIC_KEY = [
  '-----BEGIN PUBLIC KEY-----',
  'MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg=',
  '-----END PUBLIC KEY-----',
].join('\n');

const binDir = path.dirname(fileURLToPath(import.meta.url));
const base = path.dirname(binDir);
const live = path.join(base, 'agent');
const previous = path.join(base, 'agent.prev');
const journalPath = path.join(base, 'agent-swap.json');

function entry(directory) {
  return path.join(directory, 'src', 'index.mjs');
}

function exists(target) {
  try {
    fs.lstatSync(target);
    return true;
  } catch (error) {
    if (error?.code === 'ENOENT') return false;
    throw error;
  }
}

function moveIfAbsent(source, destination) {
  if (!exists(source) || exists(destination)) return false;
  fs.renameSync(source, destination);
  return true;
}

/** Restore the tree that was live before an interrupted rename sequence. */
export function recoverSwapGap() {
  if (exists(entry(live))) return { recovered: false };
  // Use the same OS-backed lock as every mutating agent command. The helper
  // is deliberately self-contained: recovery cannot import a missing live
  // module or execute code from a tree it has not verified. Never open this
  // database with ordinary file APIs or remove it; both can break exclusion.
  const dbFile = path.join(base, 'op.lock.sqlite');
  fs.accessSync(base, fs.constants.W_OK);
  if (exists(dbFile)) fs.accessSync(dbFile, fs.constants.W_OK);
  const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
  const db = new DatabaseSync(dbFile);
  try {
    db.exec('PRAGMA busy_timeout = 250');
    try { db.exec('BEGIN IMMEDIATE'); }
    catch (error) {
      if (error?.errcode === 5) throw new Error('another operation is running; recovery did not change any agent tree');
      throw error;
    }
    // An updater may have completed while this process waited for its lock.
    if (exists(entry(live))) return { recovered: false };
    return recoverLockedGap();
  } finally {
    try { db.exec('ROLLBACK'); } catch { /* no transaction if acquisition failed */ }
    db.close();
  }
}

function recoverLockedGap() {
  if (!exists(journalPath)) throw new Error(`agent entry is missing and ${journalPath} does not exist`);

  let journal;
  try {
    journal = JSON.parse(fs.readFileSync(journalPath, 'utf8'));
  } catch (error) {
    throw new Error(`agent entry is missing and the swap journal cannot be read: ${error.message}`);
  }
  const phases = journal?.operation === 'install'
    ? ['prepared', 'previous-parked', 'live-parked', 'committed']
    : ['prepared', 'live-parked', 'committed'];
  if (journal?.schema !== 1 || !['install', 'rollback'].includes(journal.operation) || !phases.includes(journal.phase)) {
    throw new Error('agent entry is missing and the swap journal is invalid');
  }

  const restore = journal.operation === 'install' ? previous : path.join(base, 'agent.rollback');
  const verified = verifyInstalledTree(restore);
  const baseline = verified.ok && !verified.signed ? verifyLocalBaseline(restore, journal.baselineSha256) : null;
  if (!verified.ok || (!verified.signed && !baseline?.ok)) {
    throw new Error(`refusing automatic recovery of the pre-swap agent: ${verified.error || baseline?.error || 'the legacy backup has no verified local baseline'}`);
  }

  if (journal.operation === 'install') {
    // agent.prev is the tree that was executing immediately before the failed
    // swap. Restore it first. A still older backup is then put back at the
    // previous name when that name is free. No candidate is deleted.
    if (!moveIfAbsent(previous, live)) throw new Error('install swap is incomplete but agent.prev cannot be restored');
    moveIfAbsent(path.join(base, 'agent.swap-old-prev'), previous);
  } else if (!moveIfAbsent(path.join(base, 'agent.rollback'), live)) {
    throw new Error('rollback swap is incomplete but agent.rollback cannot be restored');
  }

  if (!exists(entry(live))) throw new Error('swap recovery restored a tree without src/index.mjs');
  return { recovered: true, operation: journal.operation, phase: journal.phase };
}

/** An unsigned legacy tree may return only to its attested pre-swap state. */
function verifyLocalBaseline(directory, expectedHash) {
  try {
    if (!/^[0-9a-f]{64}$/.test(expectedHash ?? '')) throw new Error('the swap journal has no valid local baseline hash');
    const inventoryPath = path.join(base, 'agent-baseline.json');
    const stat = fs.lstatSync(inventoryPath);
    if (!stat.isFile() || stat.isSymbolicLink() || stat.size > 8 * 1024 * 1024) throw new Error('the local baseline is not a bounded regular file');
    const bytes = fs.readFileSync(inventoryPath);
    if (crypto.createHash('sha256').update(bytes).digest('hex') !== expectedHash) throw new Error('the local baseline does not match the swap journal');
    const inventory = JSON.parse(bytes.toString('utf8'));
    if (inventory?.schema !== 1 || !Array.isArray(inventory.files) || inventory.files.length > 10000) throw new Error('the local baseline has an unsupported shape');
    const expected = new Map();
    for (const file of inventory.files) {
      if (!safeRelative(file?.path) || !Number.isSafeInteger(file.size) || file.size < 0 || !/^[0-9a-f]{64}$/.test(file.sha256 ?? '') || expected.has(file.path)) {
        throw new Error('the local baseline contains an invalid or duplicate file');
      }
      expected.set(file.path, file);
    }
    let count = 0;
    let total = 0;
    const walk = (absolute, relative = '') => {
      const info = fs.lstatSync(absolute);
      if (info.isSymbolicLink()) throw new Error('the pre-swap tree contains a symbolic link');
      if (info.isDirectory()) {
        for (const name of fs.readdirSync(absolute)) walk(path.join(absolute, name), relative ? `${relative}/${name}` : name);
        return;
      }
      if (!info.isFile() || !safeRelative(relative)) throw new Error('the pre-swap tree contains a nonregular or unsafe file');
      const recorded = expected.get(relative);
      count += 1;
      total += info.size;
      if (count > 10000 || total > 128 * 1024 * 1024 || !recorded || recorded.size !== info.size) throw new Error('the pre-swap tree differs from its local baseline');
      const payload = fs.readFileSync(absolute);
      if (payload.length !== recorded.size || crypto.createHash('sha256').update(payload).digest('hex') !== recorded.sha256) throw new Error('the pre-swap tree differs from its local baseline');
    };
    walk(directory);
    if (count !== expected.size || !expected.has('src/index.mjs')) throw new Error('the pre-swap tree is missing baseline files');
    return { ok: true };
  } catch (error) { return { ok: false, error: error.message }; }
}

function safeRelative(relative) {
  if (typeof relative !== 'string' || relative.length === 0 || relative.length > 1024) return false;
  if (relative.startsWith('/') || relative.startsWith('\\') || /^[A-Za-z]:/.test(relative)) return false;
  return relative.split('/').every((part) => part && part !== '.' && part !== '..' && !/[\\:<\>"|?*\x00-\x1f]/.test(part) && !/[. ]$/.test(part));
}

/** Verify a signed installed tree without loading any code from that tree. */
export function verifyInstalledTree(directory) {
  const manifestFile = path.join(directory, 'MANIFEST.json');
  const signatureFile = path.join(directory, 'MANIFEST.json.sig');
  const hasManifest = exists(manifestFile);
  const hasSignature = exists(signatureFile);
  if (!hasManifest && !hasSignature) return { ok: true, signed: false };
  if (!hasManifest || !hasSignature) return { ok: false, error: 'the installed tree has only one of MANIFEST.json and MANIFEST.json.sig' };

  try {
    const manifestBytes = fs.readFileSync(manifestFile);
    const encoded = fs.readFileSync(signatureFile, 'utf8').trim();
    if (!/^[A-Za-z0-9+/]{86}==$/.test(encoded)) return { ok: false, error: 'the installed manifest signature is malformed' };
    if (!crypto.verify(null, manifestBytes, crypto.createPublicKey(PUBLIC_KEY), Buffer.from(encoded, 'base64'))) {
      return { ok: false, error: 'the installed manifest signature does not match the release key' };
    }

    const manifest = JSON.parse(manifestBytes.toString('utf8'));
    if (manifest?.schema !== 1 || typeof manifest.version !== 'string' || !Number.isInteger(manifest.contract) || !Array.isArray(manifest.files)) {
      return { ok: false, error: 'the installed manifest has an unsupported shape' };
    }

    const expected = new Map();
    for (const item of manifest.files) {
      if (!item || !safeRelative(item.path) || !/^[0-9a-f]{64}$/.test(item.sha256 ?? '') || !Number.isSafeInteger(item.size) || item.size < 0) {
        return { ok: false, error: 'the installed manifest contains an invalid file entry' };
      }
      const key = item.path.toLowerCase();
      if (expected.has(key)) return { ok: false, error: `the installed manifest lists ${item.path} more than once` };
      expected.set(key, item);
    }

    const found = new Map();
    const walk = (absolute, relative = '') => {
      const stat = fs.lstatSync(absolute);
      if (stat.isSymbolicLink()) throw new Error(`${relative || directory} is a symbolic link`);
      if (stat.isDirectory()) {
        for (const name of fs.readdirSync(absolute)) walk(path.join(absolute, name), relative ? `${relative}/${name}` : name);
        return;
      }
      if (!stat.isFile() || !safeRelative(relative)) throw new Error(`${relative} is not a safe regular file`);
      if (relative === 'MANIFEST.json' || relative === 'MANIFEST.json.sig') return;
      const key = relative.toLowerCase();
      if (found.has(key)) throw new Error(`${relative} appears more than once`);
      found.set(key, { relative, absolute, size: stat.size });
    };
    walk(directory);

    if (found.size !== expected.size) return { ok: false, error: 'the installed tree contains missing or unlisted files' };
    for (const [key, item] of expected) {
      const actual = found.get(key);
      if (!actual || actual.relative !== item.path || actual.size !== item.size) return { ok: false, error: `${item.path} does not match its manifest metadata` };
      const bytes = fs.readFileSync(actual.absolute);
      if (bytes.length !== item.size || crypto.createHash('sha256').update(bytes).digest('hex') !== item.sha256) {
        return { ok: false, error: `${item.path} does not match its manifest hash` };
      }
    }
    return { ok: true, signed: true, manifest };
  } catch (error) {
    return { ok: false, error: error.message };
  }
}

export function main(argv = process.argv.slice(2)) {
  process.env.LEGIONCTL_HOME = base;
  const recovery = recoverSwapGap();
  if (recovery.recovered) process.stderr.write(`Legion Control restored the pre-swap agent after an interrupted ${recovery.operation}.\n`);

  const verified = verifyInstalledTree(live);
  if (!verified.ok) throw new Error(`refusing to execute the agent: ${verified.error}`);

  const result = spawnSync(process.execPath, [entry(live), ...argv], { cwd: base, env: process.env, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.signal) {
    process.kill(process.pid, result.signal);
    return 1;
  }
  return result.status ?? 1;
}

if (process.env.LEGIONCTL_LAUNCHER_LIBRARY !== '1' && process.argv[1] && process.argv[1] !== '-' && fs.realpathSync(process.argv[1]) === fs.realpathSync(fileURLToPath(import.meta.url))) {
  try {
    process.exitCode = main();
  } catch (error) {
    process.stderr.write(`Legion Control launcher: ${error.message}\n`);
    process.exitCode = 1;
  }
}
