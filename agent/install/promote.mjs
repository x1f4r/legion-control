// Promote an installer-prepared tree under the same operation mutex used by
// the running agent. This module belongs to the explicitly selected installer
// source; recovery itself is performed by the separate stable launcher.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { commitInstallSwap, recoverAgentSwap } from '../src/agent-swap.mjs';
import { belowRecordedFloor, readRecordedFloor, verifyInstalledTree } from './launcher.mjs';

/**
 * The floor a signed staged release declares in its package.json (contract
 * section 13), or null when it declares none. The agent refuses a bundle whose
 * declaration is not an x.y.z version at most its own, so the installer
 * refuses the same tree rather than record a floor that tree could not meet.
 */
function declaredFloor(directory, version) {
  let floor;
  try {
    floor = JSON.parse(fs.readFileSync(path.join(directory, 'package.json'), 'utf8'))?.legionControl?.agentFloor ?? null;
  } catch (error) {
    throw new Error(`the staged agent's package.json cannot be read: ${error.message}`);
  }
  if (floor === null) return null;
  if (typeof floor !== 'string' || !/^[0-9]+\.[0-9]+\.[0-9]+$/.test(floor) || belowRecordedFloor(version, { version: floor })) {
    throw new Error(`the staged agent's package.json declares agentFloor ${JSON.stringify(floor)}, which is not an x.y.z version at most ${version}`);
  }
  return floor;
}

/**
 * After the swap commits, raise the recorded floor to `declared` when it is
 * higher, the way the agent does after an install of its own. A file that
 * cannot be read is left exactly as it is, because writing over it could
 * lower a floor nobody can see. Returns a note for stderr, or null.
 */
function raiseRecordedFloor(installBase, declared) {
  const recorded = readRecordedFloor(installBase);
  if (recorded.error) return `${recorded.error}, so the version floor was not raised`;
  if (recorded.version !== null && !belowRecordedFloor(recorded.version, { version: declared })) return null;
  const record = { schema: 1, version: declared, updatedAt: new Date().toISOString(), reason: 'install' };
  const temporary = `${recorded.file}.tmp-${process.pid}-${crypto.randomBytes(6).toString('hex')}`;
  try {
    const fd = fs.openSync(temporary, 'wx', 0o600);
    try {
      fs.writeFileSync(fd, `${JSON.stringify(record, null, 2)}\n`);
      fs.fsyncSync(fd);
    } finally { fs.closeSync(fd); }
    fs.renameSync(temporary, recorded.file);
  } catch (error) {
    try { fs.rmSync(temporary, { force: true }); } catch { /* nothing more to do */ }
    return `${recorded.file} could not be written, so the version floor was not raised: ${error.message}`;
  }
  try {
    const directory = fs.openSync(installBase, 'r');
    try { fs.fsyncSync(directory); } finally { fs.closeSync(directory); }
  } catch { /* not every platform allows a directory fsync; the rename is still atomic */ }
  return null;
}

const [baseArgument, stageArgument] = process.argv.slice(2);
if (!baseArgument || !stageArgument || !path.isAbsolute(baseArgument) || !path.isAbsolute(stageArgument)) {
  throw new Error('promotion requires an absolute install base and staging path');
}
const base = path.resolve(baseArgument);
const stage = path.resolve(stageArgument);
if (path.dirname(stage) !== base || !path.basename(stage).startsWith('agent.install.new.')) {
  throw new Error('the staged tree must be an installer-created sibling of the live agent');
}
const dbFile = path.join(base, 'op.lock.sqlite');
fs.accessSync(base, fs.constants.W_OK);
if (fs.existsSync(dbFile)) fs.accessSync(dbFile, fs.constants.W_OK);
const db = new DatabaseSync(dbFile);
try {
  db.exec('PRAGMA busy_timeout = 250');
  try { db.exec('BEGIN IMMEDIATE'); }
  catch (error) {
    if (error?.errcode === 5) throw new Error('another operation is running; the live agent and staging tree were left untouched');
    throw error;
  }
  const recovered = recoverAgentSwap(base);
  if (!recovered.ok) throw new Error(recovered.error);
  const verified = verifyInstalledTree(stage);
  if (!verified.ok) throw new Error(`staged agent verification failed: ${verified.error}`);
  // The stable launcher refuses a signed agent below the recorded version
  // floor, so promoting one would leave an install that does not start. An
  // installer has no --allow-downgrade, so the refusal says where going back
  // deliberately is done instead. An unsigned tree is a source checkout or a
  // legacy install: the launcher holds it to no floor, and its declaration
  // vouches for nothing, so it neither meets nor raises one.
  let declared = null;
  if (verified.signed) {
    const recorded = readRecordedFloor(base);
    if (belowRecordedFloor(verified.manifest.version, recorded)) {
      throw new Error(`the staged agent ${verified.manifest.version} is below the version floor ${recorded.version} recorded in ${recorded.file}, so the launcher would refuse to run it. Install a signed agent at or above ${recorded.version}; to go back deliberately, run legionctl self-update --from <bundle> --allow-downgrade from an unrestricted session, which records that bundle's version as the floor, or lower the version in ${recorded.file} by hand and run the installer again`);
    }
    declared = declaredFloor(stage, verified.manifest.version);
  }
  const ready = path.join(base, 'agent.new');
  if (fs.existsSync(ready)) throw new Error(`${ready} is retained from an earlier update; inspect it before reinstalling`);
  fs.renameSync(stage, ready);
  const result = commitInstallSwap(base);
  if (!result.ok) throw new Error(result.error);
  for (const note of result.notes ?? []) process.stderr.write(`${note}\n`);
  // Only a committed install raises the floor (contract section 13), and a
  // promotion is one. Without this an installer upgrade would leave the file
  // missing or stale, and the launcher, which reads only the file, would hold
  // the next install to nothing until the agent made one of its own.
  const raised = declared ? raiseRecordedFloor(base, declared) : null;
  if (raised) process.stderr.write(`${raised}\n`);
} finally {
  try { db.exec('ROLLBACK'); } catch { /* acquisition may have failed */ }
  db.close();
}
