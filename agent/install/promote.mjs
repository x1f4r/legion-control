// Promote an installer-prepared tree under the same operation mutex used by
// the running agent. This module belongs to the explicitly selected installer
// source; recovery itself is performed by the separate stable launcher.
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { commitInstallSwap, recoverAgentSwap } from '../src/agent-swap.mjs';
import { verifyInstalledTree } from './launcher.mjs';

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
  const ready = path.join(base, 'agent.new');
  if (fs.existsSync(ready)) throw new Error(`${ready} is retained from an earlier update; inspect it before reinstalling`);
  fs.renameSync(stage, ready);
  const result = commitInstallSwap(base);
  if (!result.ok) throw new Error(result.error);
  for (const note of result.notes ?? []) process.stderr.write(`${note}\n`);
} finally {
  try { db.exec('ROLLBACK'); } catch { /* acquisition may have failed */ }
  db.close();
}
