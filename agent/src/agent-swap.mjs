// Crash-recoverable directory swaps for agent self-updates.
//
// A rename is atomic, but a sequence of renames is not. The journal records the
// intended transaction and recovery also checks the directory layout, because
// a process can die after a rename and before the following journal write.

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

export function swapPaths(base) {
  return {
    live: path.join(base, 'agent'),
    previous: path.join(base, 'agent.prev'),
    staging: path.join(base, 'agent.new'),
    rollbackStaging: path.join(base, 'agent.rollback.new'),
    rollbackParked: path.join(base, 'agent.rollback'),
    oldPrevious: path.join(base, 'agent.swap-old-prev'),
    rollbackUsed: path.join(base, 'agent.rollback.used'),
    journal: path.join(base, 'agent-swap.json'),
    baseline: path.join(base, 'agent-baseline.json'),
  };
}

const sha256 = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

function baselineInventory(directory, io) {
  const files = [];
  let total = 0;
  const walk = (relative = '') => {
    const absolute = path.join(directory, relative);
    const stat = io.lstatSync(absolute);
    if (stat.isSymbolicLink()) throw new Error(`local baseline contains a link: ${relative}`);
    if (stat.isDirectory()) {
      for (const name of io.readdirSync(absolute)) walk(relative ? `${relative}/${name}` : name);
      return;
    }
    if (!stat.isFile()) throw new Error(`local baseline contains a nonregular file: ${relative}`);
    if (relative.length > 1024 || !relative.split('/').every((part) => part && part !== '.' && part !== '..' && !/[\\:<>"|?*\x00-\x1f]/.test(part) && !/[. ]$/.test(part))) {
      throw new Error(`local baseline contains an unsafe file path: ${relative}`);
    }
    if (files.length >= 10000 || stat.size > 64 * 1024 * 1024 || (total += stat.size) > 128 * 1024 * 1024) {
      throw new Error('local baseline exceeds size limits');
    }
    const bytes = io.readFileSync(absolute);
    if (bytes.length !== stat.size) throw new Error(`local baseline changed while being read: ${relative}`);
    files.push({ path: relative, size: bytes.length, sha256: sha256(bytes) });
  };
  walk();
  files.sort((a, b) => a.path < b.path ? -1 : a.path > b.path ? 1 : 0);
  return { schema: 1, files };
}

/** Local pre-swap identity, never a publisher signature or general rollback grant. */
function writeBaseline(base, io) {
  const p = swapPaths(base);
  if (!exists(p.live, io)) return null;
  const bytes = Buffer.from(`${JSON.stringify(baselineInventory(p.live, io))}\n`);
  const temporary = `${p.baseline}.${process.pid}.tmp`;
  io.writeFileSync(temporary, bytes, { mode: 0o600 });
  io.renameSync(temporary, p.baseline);
  return sha256(bytes);
}

export function verifyLocalBaseline(base, directory, expectedHash, { io = fs } = {}) {
  try {
    if (!/^[0-9a-f]{64}$/.test(expectedHash ?? '')) throw new Error('the journal has no local baseline identity');
    const bytes = io.readFileSync(swapPaths(base).baseline);
    if (sha256(bytes) !== expectedHash) throw new Error('the local baseline does not match its journal identity');
    const actual = Buffer.from(`${JSON.stringify(baselineInventory(directory, io))}\n`);
    if (!bytes.equals(actual)) throw new Error('the retained tree does not match the complete pre-swap local baseline');
    return { ok: true, error: null };
  } catch (error) { return { ok: false, error: error.message }; }
}

function exists(file, io) {
  try { return io.existsSync(file); } catch { return false; }
}

function writeJournal(file, value, io) {
  const temporary = `${file}.${process.pid}.tmp`;
  io.mkdirSync(path.dirname(file), { recursive: true });
  io.writeFileSync(temporary, `${JSON.stringify({ schema: 1, ...value })}\n`, { mode: 0o600 });
  io.renameSync(temporary, file);
}

function readJournal(file, io) {
  try {
    const parsed = JSON.parse(io.readFileSync(file, 'utf8'));
    if (parsed?.schema !== 1 || !['install', 'rollback'].includes(parsed.operation)) return null;
    const phases = parsed.operation === 'install'
      ? ['prepared', 'previous-parked', 'live-parked', 'committed']
      : ['prepared', 'live-parked', 'committed'];
    if (!phases.includes(parsed.phase)) return null;
    return parsed;
  } catch {
    return null;
  }
}

function clearJournal(file, io) {
  try { io.unlinkSync(file); } catch (error) { if (error?.code !== 'ENOENT') throw error; }
}

function removeDuplicate(dir, io, notes) {
  if (!exists(dir, io)) return;
  try { io.rmSync(dir, { recursive: true, force: true }); }
  catch (error) { notes.push(`${dir} is a redundant transaction copy and could not be removed: ${error.message}`); }
}

function finalizeRollback(base, io) {
  const p = swapPaths(base);
  const notes = [];
  try {
    if (exists(p.rollbackParked, io)) {
      if (exists(p.previous, io) && !exists(p.rollbackUsed, io)) io.renameSync(p.previous, p.rollbackUsed);
      if (!exists(p.previous, io)) io.renameSync(p.rollbackParked, p.previous);
    }
    if (exists(p.previous, io) && !exists(p.rollbackParked, io)) removeDuplicate(p.rollbackUsed, io, notes);
    if (exists(p.rollbackParked, io)) {
      return { ok: false, committed: true, notes, error: `the rollback is active, but the outgoing agent remains parked at ${p.rollbackParked}` };
    }
    clearJournal(p.journal, io);
    return { ok: true, committed: true, notes, error: null };
  } catch (error) {
    return { ok: false, committed: true, notes, error: `the rollback is active, but its retained backup could not be finalized: ${error.message}` };
  }
}

/** Recover or finish a swap left by a killed process. Safe to call repeatedly. */
export function recoverAgentSwap(base, { io = fs } = {}) {
  const p = swapPaths(base);
  const journal = readJournal(p.journal, io);
  if (!journal) {
    if (exists(p.journal, io)) {
      return { ok: false, committed: false, recovered: false, notes: [], error: `the swap journal at ${p.journal} is unreadable or invalid; all trees are preserved for recovery` };
    }
    const orphans = [p.rollbackParked, p.oldPrevious, p.rollbackUsed].filter((entry) => exists(entry, io));
    if (orphans.length > 0) {
      return { ok: false, committed: false, recovered: false, notes: [], error: `unowned swap backup is preserved at ${orphans.join(', ')}` };
    }
    return { ok: true, committed: false, recovered: false, notes: [], error: null };
  }

  if (journal.operation === 'rollback') {
    // live present and rollbackStaging absent proves the prepared copy was
    // renamed into place. Any later failure is backup housekeeping, not a
    // failed rollback.
    if (exists(p.live, io) && !exists(p.rollbackStaging, io)) {
      const result = finalizeRollback(base, io);
      return { ...result, recovered: true };
    }
    try {
      if (!exists(p.live, io) && exists(p.rollbackParked, io)) {
        const baseline = verifyLocalBaseline(base, p.rollbackParked, journal.baselineSha256, { io });
        if (!baseline.ok) throw new Error(baseline.error);
        io.renameSync(p.rollbackParked, p.live);
      }
      if (!exists(p.live, io)) {
        return { ok: false, committed: false, recovered: false, notes: [], error: 'rollback recovery cannot find a live or parked agent tree' };
      }
      clearJournal(p.journal, io);
      return { ok: true, committed: false, recovered: true, notes: [], error: null };
    } catch (error) {
      return { ok: false, committed: false, recovered: false, notes: [], error: `rollback recovery failed: ${error.message}` };
    }
  }

  // For install, live present and staging absent likewise proves commit.
  if (exists(p.live, io) && !exists(p.staging, io)) {
    const notes = [];
    removeDuplicate(p.oldPrevious, io, notes);
    try { clearJournal(p.journal, io); }
    catch (error) { notes.push(`the completed swap journal could not be removed: ${error.message}`); }
    return { ok: true, committed: true, recovered: true, notes, error: null };
  }
  try {
    if (!exists(p.live, io) && exists(p.previous, io)) {
      const baseline = verifyLocalBaseline(base, p.previous, journal.baselineSha256, { io });
      if (!baseline.ok) throw new Error(baseline.error);
      io.renameSync(p.previous, p.live);
    }
    if (!exists(p.previous, io) && exists(p.oldPrevious, io)) io.renameSync(p.oldPrevious, p.previous);
    if (!exists(p.live, io)) {
      return { ok: false, committed: false, recovered: false, notes: [], error: 'install recovery cannot find the live agent tree' };
    }
    clearJournal(p.journal, io);
    return { ok: true, committed: false, recovered: true, notes: [], error: null };
  } catch (error) {
    return { ok: false, committed: false, recovered: false, notes: [], error: `install recovery failed: ${error.message}` };
  }
}

/** Install agent.new, retaining the outgoing live tree as agent.prev. */
export function commitInstallSwap(base, { io = fs } = {}) {
  const p = swapPaths(base);
  const prior = recoverAgentSwap(base, { io });
  if (!prior.ok) return prior;
  if (!exists(p.staging, io)) return { ok: false, committed: false, notes: [], error: `${p.staging} does not exist` };
  if (exists(p.oldPrevious, io)) return { ok: false, committed: false, notes: [], error: `${p.oldPrevious} is preserved and needs recovery` };

  let baselineSha256;
  try {
    baselineSha256 = writeBaseline(base, io);
    writeJournal(p.journal, { operation: 'install', phase: 'prepared', baselineSha256 }, io);
    if (exists(p.previous, io)) io.renameSync(p.previous, p.oldPrevious);
    writeJournal(p.journal, { operation: 'install', phase: 'previous-parked', baselineSha256 }, io);
    if (exists(p.live, io)) io.renameSync(p.live, p.previous);
    writeJournal(p.journal, { operation: 'install', phase: 'live-parked', baselineSha256 }, io);
    io.renameSync(p.staging, p.live);
    writeJournal(p.journal, { operation: 'install', phase: 'committed', baselineSha256 }, io);
  } catch (error) {
    const recovered = recoverAgentSwap(base, { io });
    if (recovered.committed) return { ok: true, committed: true, notes: [`the install committed while reporting: ${error.message}`, ...(recovered.notes ?? [])], error: null };
    return { ok: false, committed: false, notes: recovered.notes ?? [], error: `the install swap failed: ${error.message}${recovered.ok ? '; the previous layout was restored' : `; ${recovered.error}`}` };
  }
  const recovered = recoverAgentSwap(base, { io });
  return recovered.ok
    ? { ok: true, committed: true, notes: recovered.notes ?? [], error: null }
    : { ok: true, committed: true, notes: [recovered.error, ...(recovered.notes ?? [])], error: null };
}

/** Activate a verified copy at agent.rollback.new and retain outgoing live. */
export function commitRollbackSwap(base, { io = fs } = {}) {
  const p = swapPaths(base);
  const prior = recoverAgentSwap(base, { io });
  if (!prior.ok) return prior;
  if (!exists(p.rollbackStaging, io)) return { ok: false, committed: false, notes: [], error: `${p.rollbackStaging} does not exist` };
  if (exists(p.rollbackParked, io) || exists(p.rollbackUsed, io)) {
    return { ok: false, committed: false, notes: [], error: 'a retained rollback backup is preserved and needs recovery' };
  }

  let baselineSha256;
  try {
    baselineSha256 = writeBaseline(base, io);
    writeJournal(p.journal, { operation: 'rollback', phase: 'prepared', baselineSha256 }, io);
    if (exists(p.live, io)) io.renameSync(p.live, p.rollbackParked);
    writeJournal(p.journal, { operation: 'rollback', phase: 'live-parked', baselineSha256 }, io);
    io.renameSync(p.rollbackStaging, p.live);
    writeJournal(p.journal, { operation: 'rollback', phase: 'committed', baselineSha256 }, io);
  } catch (error) {
    const recovered = recoverAgentSwap(base, { io });
    if (recovered.committed) return { ok: true, committed: true, notes: [`the rollback committed while reporting: ${error.message}`, ...(recovered.notes ?? [])], error: null };
    return { ok: false, committed: false, notes: recovered.notes ?? [], error: `the rollback swap failed: ${error.message}${recovered.ok ? '; the original layout was restored' : `; ${recovered.error}`}` };
  }

  const finalized = finalizeRollback(base, io);
  return finalized.ok
    ? { ok: true, committed: true, notes: finalized.notes, error: null }
    : { ok: true, committed: true, notes: [finalized.error, ...finalized.notes], error: null };
}
